use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::Rc,
};

use leptos::prelude::*;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

use crate::bridge::web::idb;
use crate::{
    bridge::{Accepted, LocalSubmissionId, SendRejected},
    state::{
        ActiveAgentRef, ActiveProjectRef, AppState, LocalHostId, MobileShellError, MobileTab,
        PendingImage, PendingSubmission, PendingSubmissionState, SubmissionOriginId,
        SubmissionTarget,
    },
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecoveryStatus {
    Disabled,
    Loading,
    Ready,
    Failed,
    RestoreFailed,
}

#[derive(Clone, Serialize, Deserialize)]
struct Journal {
    version: u32,
    revision: u64,
    host: Option<LocalHostId>,
    tab: MobileTab,
    viewing_chat: bool,
    origin: MobileTab,
    parents: Vec<ActiveAgentRef>,
    agent: Option<ActiveAgentRef>,
    project: Option<ActiveProjectRef>,
    draft_id: uuid::Uuid,
    edit_id: uuid::Uuid,
    text: String,
    images: Vec<PendingImage>,
    backend: Option<protocol::BackendKind>,
    custom_agent: Option<protocol::CustomAgentId>,
    settings: protocol::SessionSettingsValues,
    pending: Vec<PendingSubmission>,
}

impl Journal {
    fn capture(state: &AppState, revision: u64) -> Self {
        let host = state.active_local_host_id.get();
        let mut pending = state
            .pending_submissions
            .get()
            .into_values()
            .filter(|record| Some(&record.local_host_id) == host.as_ref())
            .collect::<Vec<_>>();
        pending.sort_by_key(|record| record.local_submission_id.0);
        let mut latest = HashMap::new();
        for record in pending {
            latest.insert(record.origin, record);
        }
        let mut pending = latest.into_values().collect::<Vec<_>>();
        pending.sort_by_key(|record| record.local_submission_id.0);
        Self {
            version: 1,
            revision,
            host: state.active_local_host_id.get(),
            tab: state.active_tab.get(),
            viewing_chat: state.viewing_chat.get(),
            origin: state.chat_origin_tab.get(),
            parents: state.chat_back_stack.get(),
            agent: state.active_agent.get(),
            project: state.active_project.get(),
            draft_id: state.chat_draft_id.get(),
            edit_id: state.chat_edit_id.get(),
            text: state.chat_input.get(),
            images: state.chat_images.get(),
            backend: state.draft_backend_override.get(),
            custom_agent: state.draft_custom_agent_id.get(),
            settings: state.draft_session_settings.get(),
            pending,
        }
    }

    fn restore(mut self, state: &AppState) -> Result<(), String> {
        if self.version != 1 {
            return Err("Input recovery journal version is unsupported".into());
        }
        if let Some(host) = &self.host {
            let valid_agent = self
                .agent
                .as_ref()
                .is_none_or(|agent| agent.local_host_id == *host);
            let valid_project = self
                .project
                .as_ref()
                .is_none_or(|project| project.local_host_id == *host);
            let valid_parents = self
                .parents
                .iter()
                .all(|agent| agent.local_host_id == *host);
            let valid_pending = self.pending.iter().all(|record| {
                record.local_host_id == *host
                    && match &record.target {
                        SubmissionTarget::NewChat => true,
                        SubmissionTarget::Agent(agent) => agent.local_host_id == *host,
                    }
            });
            if !valid_agent || !valid_project || !valid_parents || !valid_pending {
                return Err("Input recovery contains a different host's ownership".into());
            }
        }
        let mut next_origin = 0;
        let mut next_attempt = 0;
        for record in &mut self.pending {
            record.connection_instance_id = None;
            if record.state == PendingSubmissionState::QueuedLocally {
                record.state = PendingSubmissionState::DeliveryUnknown;
            }
            next_origin = next_origin.max(
                record
                    .origin
                    .0
                    .checked_add(1)
                    .ok_or("Recovered input identity exhausted")?,
            );
            next_attempt = next_attempt.max(
                record
                    .local_submission_id
                    .0
                    .checked_add(1)
                    .ok_or("Recovered attempt identity exhausted")?,
            );
        }
        crate::bridge::web::reserve_recovered_submission_ids(next_attempt);
        batch(|| {
            state.active_local_host_id.set(self.host);
            state.active_tab.set(self.tab);
            state.viewing_chat.set(self.viewing_chat);
            state.chat_origin_tab.set(self.origin);
            state.chat_back_stack.set(self.parents);
            state.active_agent.set(self.agent);
            state.active_project.set(self.project);
            state.chat_draft_id.set(self.draft_id);
            state.set_chat_input(self.text);
            state.chat_edit_id.set(self.edit_id);
            state.chat_images.set(self.images);
            state.draft_backend_override.set(self.backend);
            state.draft_custom_agent_id.set(self.custom_agent);
            state.draft_session_settings.set(self.settings);
            state.pending_submissions.set(
                self.pending
                    .into_iter()
                    .map(|record| (record.local_submission_id, record))
                    .collect(),
            );
            state.next_submission_origin.set(next_origin);
        });
        Ok(())
    }
}

struct Write {
    journal: Journal,
    reply: Option<oneshot::Sender<Result<(), String>>>,
}

#[derive(Clone)]
pub struct CapturedDraft {
    pub(crate) host: Option<LocalHostId>,
    pub(crate) agent: Option<ActiveAgentRef>,
    target: SubmissionTarget,
    pub(crate) draft_id: uuid::Uuid,
    pub(crate) edit_id: uuid::Uuid,
    pub(crate) text: String,
    images: Vec<PendingImage>,
    project: Option<ActiveProjectRef>,
    backend: Option<protocol::BackendKind>,
    custom_agent: Option<protocol::CustomAgentId>,
    settings: protocol::SessionSettingsValues,
    source_session: Option<protocol::SessionId>,
}

impl CapturedDraft {
    pub fn capture(state: &AppState, target: SubmissionTarget) -> Self {
        let agent = state.active_agent.get_untracked();
        let source_session = agent.as_ref().and_then(|active| {
            state.agents.with_untracked(|agents| {
                agents
                    .iter()
                    .find(|info| info.agent_ref() == active.as_agent_ref())
                    .and_then(|info| info.session_id.clone())
            })
        });
        Self {
            host: state.active_local_host_id.get_untracked(),
            agent,
            target,
            draft_id: state.chat_draft_id.get_untracked(),
            edit_id: state.chat_edit_id.get_untracked(),
            text: state.chat_input.get_untracked(),
            images: state.chat_images.get_untracked(),
            project: state.active_project.get_untracked(),
            backend: state.draft_backend_override.get_untracked(),
            custom_agent: state.draft_custom_agent_id.get_untracked(),
            settings: state.draft_session_settings.get_untracked(),
            source_session,
        }
    }

    pub fn clear_options(&self, state: &AppState) {
        if self.target == SubmissionTarget::NewChat && self.agent.is_none() {
            state.draft_backend_override.set(None);
            state.draft_custom_agent_id.set(None);
            state
                .draft_session_settings
                .set(protocol::SessionSettingsValues::default());
        }
    }

    pub fn matches(&self, state: &AppState) -> bool {
        self.host == state.active_local_host_id.get_untracked()
            && self.agent == state.active_agent.get_untracked()
            && self.draft_id == state.chat_draft_id.get_untracked()
            && self.edit_id == state.chat_edit_id.get_untracked()
            && self.text == state.chat_input.get_untracked()
            && self.images == state.chat_images.get_untracked()
            && self.project == state.active_project.get_untracked()
            && self.backend == state.draft_backend_override.get_untracked()
            && self.custom_agent == state.draft_custom_agent_id.get_untracked()
            && self.settings == state.draft_session_settings.get_untracked()
    }
}

#[derive(Clone)]
pub struct InputCustody {
    pub id: protocol::HumanInputId,
    pub origin: Option<SubmissionOriginId>,
    pub draft: Option<CapturedDraft>,
}

impl InputCustody {
    pub fn retry(record: &PendingSubmission) -> Self {
        Self {
            id: record.human_input_id.clone(),
            origin: Some(record.origin),
            draft: None,
        }
    }
}

#[derive(Clone)]
struct Runtime {
    state: AppState,
    sender: mpsc::UnboundedSender<Write>,
    revision: Rc<Cell<u64>>,
    prepared: Rc<RefCell<HashMap<LocalSubmissionId, Option<CapturedDraft>>>>,
}

thread_local! { static RUNTIME: RefCell<Option<Runtime>> = const { RefCell::new(None) }; }

fn fail(state: &AppState, message: String) {
    state.recovery_status.set(RecoveryStatus::Failed);
    state.mobile_shell_error.set(Some(MobileShellError {
        code: protocol::MobileAccessErrorCode::StoreLoadFailed,
        message,
    }));
}

impl Runtime {
    fn enqueue(&self, reply: Option<oneshot::Sender<Result<(), String>>>) -> Result<(), String> {
        let revision = self
            .revision
            .get()
            .checked_add(1)
            .ok_or("Input recovery revision exhausted")?;
        self.revision.set(revision);
        let mut journal = untrack(|| Journal::capture(&self.state, revision));
        // Durable custody moves the captured draft in the same transaction,
        // even while the visible composer is awaiting transport admission.
        if self.prepared.borrow().values().flatten().any(|draft| {
            draft.host == journal.host
                && draft.agent == journal.agent
                && draft.draft_id == journal.draft_id
                && draft.edit_id == journal.edit_id
                && draft.project == journal.project
                && draft.backend == journal.backend
                && draft.custom_agent == journal.custom_agent
                && draft.settings == journal.settings
                && draft.text == journal.text
                && draft.images == journal.images
        }) {
            journal.text.clear();
            journal.images.clear();
        }
        self.sender
            .send(Write { journal, reply })
            .map_err(|_| "Input recovery writer is unavailable".into())
    }

    async fn commit(&self) -> Result<(), String> {
        let (sender, receiver) = oneshot::channel();
        self.enqueue(Some(sender))?;
        receiver
            .await
            .map_err(|_| "Input recovery transaction completion was lost".to_owned())?
    }
}

// Only the production App installs this lifetime. Standalone component fixtures
// above this boundary do not install an app journal or claim recovery coverage.
pub fn begin(state: &AppState) {
    state.recovery_status.set(RecoveryStatus::Loading);
    let (sender, mut receiver) = mpsc::unbounded_channel::<Write>();
    let runtime = Runtime {
        state: state.clone(),
        sender,
        revision: Rc::new(Cell::new(0)),
        prepared: Rc::new(RefCell::new(HashMap::new())),
    };
    RUNTIME.with(|slot| {
        *slot.borrow_mut() = Some(runtime);
    });
    let writer_state = state.clone();
    wasm_bindgen_futures::spawn_local(async move {
        let mut committed = 0;
        while let Some(write) = receiver.recv().await {
            let result: Result<(), String> = async {
                if write.journal.revision <= committed {
                    return Err("Input recovery writes arrived out of order".into());
                }
                let value = serde_json::to_string(&write.journal)
                    .map_err(|_| "Input recovery could not be encoded".to_owned())?;
                let key = format!(
                    "{}{}",
                    idb::RECOVERY_KEY_PREFIX,
                    serde_json::to_string(&write.journal.host)
                        .map_err(|_| "Input recovery host key could not be encoded".to_owned())?
                );
                idb::put(idb::STORE_RECOVERY, &key, &value).await?;
                committed = write.journal.revision;
                Ok(())
            }
            .await;
            if let Err(error) = &result {
                fail(&writer_state, error.clone());
            }
            if let Some(reply) = write.reply {
                let _ = reply.send(result);
            }
        }
    });
    let status = state.recovery_status;
    on_cleanup(move || {
        RUNTIME.with(|slot| {
            if slot
                .borrow()
                .as_ref()
                .is_some_and(|runtime| runtime.state.recovery_status == status)
            {
                slot.borrow_mut().take();
            }
        })
    });
}

pub async fn restore(state: &AppState) -> Result<(), String> {
    let runtime = RUNTIME
        .with(|slot| slot.borrow().clone())
        .ok_or("Input recovery is not initialized")?;
    let selection = state.active_local_host_id.get_untracked();
    let hosts = crate::bridge::list_paired_hosts().await?;
    let known = hosts
        .iter()
        .map(|host| host.local_host_id.clone())
        .collect::<std::collections::HashSet<_>>();
    for key in idb::keys(idb::STORE_RECOVERY).await? {
        let Some(owner_key) = key.strip_prefix(idb::RECOVERY_KEY_PREFIX) else {
            continue;
        };
        let owner: Option<LocalHostId> =
            serde_json::from_str(owner_key).map_err(|_| "Input recovery host key is invalid")?;
        if owner.is_some_and(|owner| !known.contains(&owner)) {
            idb::delete(idb::STORE_RECOVERY, &key).await?;
        }
    }
    let key = format!(
        "{}{}",
        idb::RECOVERY_KEY_PREFIX,
        serde_json::to_string(&selection)
            .map_err(|_| "Input recovery host key could not be encoded")?
    );
    if let Some(value) = idb::get(idb::STORE_RECOVERY, &key).await? {
        let journal: Journal = serde_json::from_str(&value)
            .map_err(|_| "Input recovery journal could not be decoded")?;
        if journal.host != selection {
            return Err("Input recovery belongs to a different host".into());
        }
        runtime.revision.set(journal.revision);
        journal.restore(state)?;
    }
    state.recovery_status.set(RecoveryStatus::Ready);
    Ok(())
}

pub fn observe(state: &AppState) {
    let state = state.clone();
    Effect::new(move |_| {
        if state.recovery_status.get() != RecoveryStatus::Ready {
            return;
        }
        // Read tracked user-owned values, never connection status or transcript.
        let _ = Journal::capture(&state, 0);
        let runtime = RUNTIME.with(|slot| slot.borrow().clone());
        if let Some(runtime) = runtime
            && let Err(error) = runtime.enqueue(None)
        {
            fail(&state, error);
        }
    });
}

pub async fn prepare_transport(
    host: &LocalHostId,
    line: &str,
    accepted: Accepted,
    custody: Option<InputCustody>,
) -> Result<(), SendRejected> {
    let Some(runtime) = RUNTIME.with(|slot| slot.borrow().clone()) else {
        return Ok(());
    };
    let result: Result<(), String> = async {
        let envelope: protocol::Envelope = serde_json::from_str(line)
            .map_err(|_| "Input custody frame could not be decoded".to_owned())?;
        let requested_input_id = custody.as_ref().map(|input| input.id.clone());
        let (human_input_id, action, target, input, spawn_payload) = match envelope.kind {
            protocol::FrameKind::SendMessage
            | protocol::FrameKind::SteerMessage
            | protocol::FrameKind::HumanInput => {
                let (id, action, input) = if envelope.kind == protocol::FrameKind::HumanInput {
                    let payload = envelope
                        .parse_payload::<protocol::HumanInputPayload>()
                        .map_err(|_| "Input custody payload is invalid")?;
                    (payload.id, payload.action, payload.input)
                } else {
                    (
                        requested_input_id.unwrap_or_else(|| {
                            protocol::HumanInputId(uuid::Uuid::new_v4().to_string())
                        }),
                        if envelope.kind == protocol::FrameKind::SendMessage {
                            protocol::HumanInputAction::Send
                        } else {
                            protocol::HumanInputAction::Steer
                        },
                        envelope
                            .parse_payload::<protocol::SendMessagePayload>()
                            .map_err(|_| "Input custody payload is invalid")?,
                    )
                };
                let owners = runtime.state.agents.with_untracked(|agents| {
                    agents
                        .iter()
                        .filter(|a| {
                            a.local_host_id == *host && a.instance_stream == envelope.stream
                        })
                        .map(|a| a.agent_ref())
                        .collect::<Vec<_>>()
                });
                let [owner] = owners.as_slice() else {
                    return Err(
                        "Input custody requires the exact current agent stream owner".into(),
                    );
                };
                (
                    id,
                    action,
                    SubmissionTarget::Agent(owner.clone()),
                    input,
                    None,
                )
            }
            protocol::FrameKind::SpawnAgent => {
                let payload = envelope
                    .parse_payload::<protocol::SpawnAgentPayload>()
                    .map_err(|_| "New-chat custody payload is invalid")?;
                let (message, images) = match &payload.params {
                    protocol::SpawnAgentParams::New { prompt, images, .. }
                    | protocol::SpawnAgentParams::Fork { prompt, images, .. } => {
                        (prompt.clone(), images.clone())
                    }
                    protocol::SpawnAgentParams::Resume { prompt, .. } => {
                        (prompt.clone().unwrap_or_default(), None)
                    }
                };
                (
                    requested_input_id.unwrap_or_else(|| protocol::HumanInputId(uuid::Uuid::new_v4().to_string())),
                    protocol::HumanInputAction::Send,
                    SubmissionTarget::NewChat,
                    protocol::SendMessagePayload {
                        message,
                        images,
                        origin: None,
                        tool_response: None,
                    },
                    Some(payload),
                )
            }
            _ => return Ok(()),
        };
        if runtime.state.active_local_host_id.get_untracked().as_ref() != Some(host) {
            return Err("The selected host changed; this request was not transmitted".into());
        }
        if runtime.state.recovery_status.get_untracked() != RecoveryStatus::Ready {
            return Err("Input recovery is unavailable; this request was not transmitted".into());
        }
        if !runtime.state.can_hold_submission_untracked(host) {
            return Err("Input recovery is full; resolve held requests before sending".into());
        }
        let origin = custody.as_ref().and_then(|input| input.origin).or_else(|| runtime
            .state
            .pending_submissions
            .with_untracked(|records| {
                records
                    .values()
                    .find(|record| {
                        record.local_host_id == *host
                            && record.human_input_id == human_input_id
                            && record.target == target
                    })
                    .map(|record| record.origin)
            }))
            .unwrap_or_else(|| runtime.state.mint_submission_origin());
        let record = PendingSubmission {
            human_input_id,
            failure_reason: None,
            action,
            local_submission_id: accepted.local_submission_id,
            origin,
            local_host_id: host.clone(),
            connection_instance_id: Some(accepted.connection_instance_id),
            spawn_payload,
            target,
            text: input.message,
            images: input.images.unwrap_or_default(),
            tool_response: input.tool_response,
            state: PendingSubmissionState::QueuedLocally,
        };
        let captured = custody.and_then(|input| input.draft);
        if let Some(draft) = &captured {
            let images = draft.images.iter().map(|image| protocol::ImageData { media_type: image.media_type.clone(), data: image.data.clone() }).collect::<Vec<_>>();
            if draft.host.as_ref() != Some(host) || draft.target != record.target || draft.text.trim() != record.text || images != record.images {
                return Err("Input custody does not match the originating draft".into());
            }
            if let Some(payload) = &record.spawn_payload {
                let matches = match &payload.params {
                    protocol::SpawnAgentParams::Fork { from_session_id, .. } => draft.source_session.as_ref() == Some(from_session_id),
                    protocol::SpawnAgentParams::New { backend_kind, session_settings, .. } => draft.agent.is_none()
                        && draft.backend.is_none_or(|backend| backend == *backend_kind)
                        && draft.project.as_ref().map(|project| &project.project_id) == payload.project_id.as_ref()
                        && draft.custom_agent == payload.custom_agent_id
                        && draft.settings == session_settings.clone().unwrap_or_default(),
                    protocol::SpawnAgentParams::Resume { .. } => false,
                };
                if !matches { return Err("The originating draft's launch settings changed; request was not transmitted".into()); }
            }
        }
        runtime
            .prepared
            .borrow_mut()
            .insert(accepted.local_submission_id, captured);
        runtime.state.pending_submissions.update(|records| {
            records.insert(accepted.local_submission_id, record);
        });
        if let Err(error) = runtime.commit().await {
            runtime.state.pending_submissions.update(|records| {
                records.remove(&accepted.local_submission_id);
            });
            runtime
                .prepared
                .borrow_mut()
                .remove(&accepted.local_submission_id);
            return Err(error);
        }
        Ok(())
    }
    .await;
    result.map_err(|message| {
        runtime.state.mobile_shell_error.set(Some(MobileShellError {
            code: protocol::MobileAccessErrorCode::StoreLoadFailed,
            message,
        }));
        SendRejected::InputCustodyUnavailable
    })
}

pub async fn retain_rejected_input(
    host: &LocalHostId,
    kind: protocol::FrameKind,
    payload: serde_json::Value,
    custody: Option<InputCustody>,
    uncertain: bool,
) {
    let Some(runtime) = RUNTIME.with(|slot| slot.borrow().clone()) else {
        return;
    };
    let Some(InputCustody {
        id,
        origin: Some(origin),
        draft: Some(draft),
    }) = custody
    else {
        return;
    };
    if draft.matches(&runtime.state)
        || runtime.state.pending_submissions.with_untracked(|records| {
            records
                .values()
                .any(|record| record.local_host_id == *host && record.origin == origin)
        })
    {
        return;
    }
    let decoded = match kind {
        protocol::FrameKind::SpawnAgent => {
            serde_json::from_value::<protocol::SpawnAgentPayload>(payload)
                .map(|spawn| (protocol::HumanInputAction::Send, Some(spawn)))
                .map_err(|_| "Rejected launch input could not be retained")
        }
        protocol::FrameKind::HumanInput => {
            serde_json::from_value::<protocol::HumanInputPayload>(payload)
                .map(|input| (input.action, None))
                .map_err(|_| "Rejected message input could not be retained")
        }
        protocol::FrameKind::SendMessage => Ok((protocol::HumanInputAction::Send, None)),
        protocol::FrameKind::SteerMessage => Ok((protocol::HumanInputAction::Steer, None)),
        _ => return,
    };
    let (action, spawn_payload) = match decoded {
        Ok(decoded) => decoded,
        Err(error) => {
            fail(&runtime.state, error.into());
            return;
        }
    };
    let minimum = runtime
        .state
        .pending_submissions
        .with_untracked(|records| records.keys().map(|id| id.0 + 1).max().unwrap_or(0));
    let local_submission_id = crate::bridge::web::reserve_unsent_submission_id(minimum);
    crate::bridge::web::reserve_recovered_submission_ids(local_submission_id.0 + 1);
    runtime.state.pending_submissions.update(|records| {
        records.insert(
            local_submission_id,
            PendingSubmission {
                human_input_id: id,
                origin,
                local_host_id: host.clone(),
                local_submission_id,
                connection_instance_id: None,
                spawn_payload,
                failure_reason: None,
                action,
                target: draft.target,
                text: draft.text.trim().to_owned(),
                images: draft
                    .images
                    .into_iter()
                    .map(|image| protocol::ImageData {
                        media_type: image.media_type,
                        data: image.data,
                    })
                    .collect(),
                tool_response: None,
                state: if uncertain {
                    PendingSubmissionState::DeliveryUnknown
                } else {
                    PendingSubmissionState::NotSent
                },
            },
        );
    });
    if let Err(error) = runtime.commit().await {
        fail(&runtime.state, error);
    }
}

pub fn finish_admission(
    state: &AppState,
    mut record: PendingSubmission,
) -> Option<PendingSubmission> {
    let prepared = RUNTIME.with(|slot| {
        slot.borrow()
            .as_ref()
            .filter(|runtime| runtime.state.recovery_status == state.recovery_status)
            .is_some_and(|runtime| {
                runtime
                    .prepared
                    .borrow_mut()
                    .remove(&record.local_submission_id)
                    .is_some()
            })
    });
    let current = state
        .pending_submissions
        .with_untracked(|records| records.get(&record.local_submission_id).cloned());
    if prepared && current.is_none() {
        return None;
    }
    if let Some(current) = current {
        record.spawn_payload = current.spawn_payload;
        if prepared {
            record.state = current.state;
        }
    }
    Some(record)
}

pub fn hold_spawn(
    state: &AppState,
    host: &LocalHostId,
    accepted: Accepted,
    payload: &protocol::SpawnAgentPayload,
) {
    let prepared = RUNTIME.with(|slot| {
        slot.borrow().as_ref().is_some_and(|runtime| {
            runtime
                .prepared
                .borrow()
                .contains_key(&accepted.local_submission_id)
        })
    });
    if state
        .pending_submissions
        .with_untracked(|records| records.contains_key(&accepted.local_submission_id))
    {
        state.pending_submissions.update(|records| {
            if let Some(record) = records.get_mut(&accepted.local_submission_id) {
                record.spawn_payload = Some(payload.clone());
            }
        });
        return;
    }
    if prepared {
        return;
    }
    let (text, images) = match &payload.params {
        protocol::SpawnAgentParams::New { prompt, images, .. }
        | protocol::SpawnAgentParams::Fork { prompt, images, .. } => {
            (prompt.clone(), images.clone().unwrap_or_default())
        }
        protocol::SpawnAgentParams::Resume { prompt, .. } => {
            (prompt.clone().unwrap_or_default(), Vec::new())
        }
    };
    state.pending_submissions.update(|records| {
        records.insert(
            accepted.local_submission_id,
            PendingSubmission {
                human_input_id: protocol::HumanInputId(uuid::Uuid::new_v4().to_string()),
                failure_reason: None,
                action: protocol::HumanInputAction::Send,
                local_submission_id: accepted.local_submission_id,
                origin: state.mint_submission_origin(),
                local_host_id: host.clone(),
                connection_instance_id: Some(accepted.connection_instance_id),
                spawn_payload: Some(payload.clone()),
                target: SubmissionTarget::NewChat,
                text,
                images,
                tool_response: None,
                state: PendingSubmissionState::QueuedLocally,
            },
        );
    });
}

pub async fn reject_prepared(accepted: Accepted) {
    let runtime = RUNTIME.with(|slot| slot.borrow().clone());
    if let Some(runtime) = runtime {
        let captured = runtime
            .prepared
            .borrow_mut()
            .remove(&accepted.local_submission_id);
        if let Some(captured) = captured {
            let return_to_composer = captured
                .as_ref()
                .is_some_and(|draft| draft.matches(&runtime.state));
            runtime.state.pending_submissions.update(|records| {
                let previous_attempt = captured.is_none()
                    && records
                        .get(&accepted.local_submission_id)
                        .is_some_and(|current| {
                            records.values().any(|previous| {
                                previous.local_submission_id != current.local_submission_id
                                    && previous.local_host_id == current.local_host_id
                                    && previous.origin == current.origin
                            })
                        });
                if return_to_composer || previous_attempt {
                    records.remove(&accepted.local_submission_id);
                } else if let Some(record) = records.get_mut(&accepted.local_submission_id) {
                    record.state = PendingSubmissionState::NotSent;
                    record.connection_instance_id = None;
                    let origin = record.origin;
                    records.retain(|id, record| {
                        *id == accepted.local_submission_id || record.origin != origin
                    });
                }
            });
            if let Err(error) = runtime.commit().await {
                fail(&runtime.state, error);
            }
        }
    }
}

pub fn report_restore_failure(state: &AppState, error: String) {
    fail(state, error);
    state.recovery_status.set(RecoveryStatus::RestoreFailed);
}

pub fn report_write_failure(state: &AppState, error: String) {
    fail(state, error);
}

pub async fn retry(state: &AppState) {
    match state.recovery_status.get_untracked() {
        RecoveryStatus::RestoreFailed => {
            state.recovery_status.set(RecoveryStatus::Loading);
            crate::app::restore_native_document(state.clone()).await;
        }
        RecoveryStatus::Failed => match commit_current().await {
            Ok(()) => batch(|| {
                state.recovery_status.set(RecoveryStatus::Ready);
                state.mobile_shell_error.set(None);
            }),
            Err(error) => fail(state, error),
        },
        _ => {}
    }
}

pub async fn commit_current() -> Result<(), String> {
    let runtime = RUNTIME
        .with(|slot| slot.borrow().clone())
        .ok_or("Input recovery is not initialized")?;
    runtime.commit().await
}
