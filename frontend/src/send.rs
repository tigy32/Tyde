use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

use protocol::types::{AgentCompactPayload, TeamCompactPayload};
use protocol::{
    AgentGroupsUpdate, AgentPinsUpdate, AgentTagsUpdate, AgentsSmartViewsUpdate,
    AgentsViewPreferencesUpdate, CancelWorkflowPayload, CloseAgentPayload, CustomAgent,
    CustomAgentDeletePayload, CustomAgentId, CustomAgentUpsertPayload, Envelope, FrameKind,
    ImageData, McpServerConfig, McpServerDeletePayload, McpServerId, McpServerUpsertPayload,
    MobileDeviceId, MobileDeviceRevokePayload, MobilePairingCancelPayload, MobilePairingOfferId,
    MobilePairingStartPayload, ProjectId, SetAgentGroupsPayload, SetAgentPinsPayload,
    SetAgentTagsPayload, SetAgentsSmartViewsPayload, SetAgentsViewPreferencesPayload,
    SkillRefreshPayload, Steering, SteeringDeletePayload, SteeringId, SteeringUpsertPayload,
    StreamPath, TeamDeletePayload, TeamDraftApplyTemplatePayload, TeamDraftCommitPayload,
    TeamDraftCreatePayload, TeamDraftDiscardPayload, TeamDraftId, TeamDraftMemberEdit,
    TeamDraftMemberId, TeamDraftShufflePayload, TeamDraftShuffleScope, TeamDraftUpdatePayload,
    TeamId, TeamMemberActivatePayload, TeamMemberCreatePayload, TeamMemberCreateSpec,
    TeamMemberDeletePayload, TeamMemberId, TeamMemberShufflePayload, TeamMemberUpdatePayload,
    TeamSetManagerPayload, TeamTemplateId, TriggerWorkflowPayload, WorkflowId,
    WorkflowRefreshPayload, WorkflowRunId,
};
use serde::Serialize;
use serde_json::Value;

use crate::bridge;

// WASM is single-threaded, so RefCell is fine.
// Per-stream monotonic sequence numbers, as required by the protocol.
type SequenceKey = (String, StreamPath);

#[derive(Clone, Copy)]
struct SequenceReservation {
    seq: u64,
    host_epoch: u64,
}

struct SequenceCursor {
    next: u64,
    host_epoch: u64,
}

// Tauri invocation always yields to a Promise. Keep that yield inside a
// per-stream FIFO gate so reservations reach the native router in protocol
// order without blocking unrelated streams.
#[derive(Default)]
struct SendLockState {
    held: bool,
    waiters: VecDeque<SendWaiter>,
}

struct SendWaiter {
    id: u64,
    waker: Waker,
}

thread_local! {
    static SEQ_MAP: RefCell<HashMap<SequenceKey, SequenceCursor>> = RefCell::new(HashMap::new());
    static HOST_EPOCHS: RefCell<HashMap<String, u64>> = RefCell::new(HashMap::new());
    static SEND_LOCKS: RefCell<HashMap<SequenceKey, SendLockState>> =
        RefCell::new(HashMap::new());
    static NEXT_SEND_WAITER_ID: Cell<u64> = const { Cell::new(0) };
}

#[cfg(all(test, target_arch = "wasm32"))]
fn current_seq(host_id: &str, stream: &StreamPath) -> u64 {
    let host_epoch =
        HOST_EPOCHS.with(|epochs| epochs.borrow().get(host_id).copied().unwrap_or(0));
    SEQ_MAP.with(|map| {
        map.borrow()
            .get(&(host_id.to_owned(), stream.clone()))
            .filter(|cursor| cursor.host_epoch == host_epoch)
            .map(|cursor| cursor.next)
            .unwrap_or(0)
    })
}

fn reserve_seq(host_id: &str, stream: &StreamPath) -> SequenceReservation {
    let host_epoch =
        HOST_EPOCHS.with(|epochs| epochs.borrow().get(host_id).copied().unwrap_or(0));
    SEQ_MAP.with(|map| {
        let mut map = map.borrow_mut();
        let cursor = map
            .entry((host_id.to_owned(), stream.clone()))
            .or_insert(SequenceCursor {
                next: 0,
                host_epoch,
            });
        if cursor.host_epoch != host_epoch {
            *cursor = SequenceCursor {
                next: 0,
                host_epoch,
            };
        }
        let seq = cursor.next;
        cursor.next = seq.wrapping_add(1);
        SequenceReservation { seq, host_epoch }
    })
}

fn release_seq_if_last(
    host_id: &str,
    stream: &StreamPath,
    reservation: SequenceReservation,
) {
    SEQ_MAP.with(|map| {
        let mut map = map.borrow_mut();
        let key = (host_id.to_owned(), stream.clone());
        let Some(cursor) = map.get_mut(&key) else {
            return;
        };
        if cursor.host_epoch == reservation.host_epoch
            && cursor.next == reservation.seq.wrapping_add(1)
        {
            cursor.next = reservation.seq;
        }
    });
}

struct SendLockFuture {
    key: SequenceKey,
    waiter_id: Option<u64>,
}

struct SendLockGuard {
    key: SequenceKey,
}

fn acquire_send_lock(host_id: &str, stream: &StreamPath) -> SendLockFuture {
    SendLockFuture {
        key: (host_id.to_owned(), stream.clone()),
        waiter_id: None,
    }
}

impl Future for SendLockFuture {
    type Output = SendLockGuard;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let acquired = SEND_LOCKS.with(|locks| {
            let mut locks = locks.borrow_mut();
            let state = locks.entry(this.key.clone()).or_default();
            match this.waiter_id {
                None if !state.held && state.waiters.is_empty() => {
                    state.held = true;
                    true
                }
                None => {
                    let waiter_id = NEXT_SEND_WAITER_ID.with(|next| {
                        let waiter_id = next.get();
                        next.set(waiter_id.wrapping_add(1));
                        waiter_id
                    });
                    state.waiters.push_back(SendWaiter {
                        id: waiter_id,
                        waker: cx.waker().clone(),
                    });
                    this.waiter_id = Some(waiter_id);
                    false
                }
                Some(waiter_id)
                    if !state.held
                        && state.waiters.front().map(|waiter| waiter.id) == Some(waiter_id) =>
                {
                    state.waiters.pop_front();
                    state.held = true;
                    this.waiter_id = None;
                    true
                }
                Some(waiter_id) => {
                    if let Some(waiter) = state
                        .waiters
                        .iter_mut()
                        .find(|waiter| waiter.id == waiter_id)
                    {
                        waiter.waker = cx.waker().clone();
                    }
                    false
                }
            }
        });
        if acquired {
            Poll::Ready(SendLockGuard {
                key: this.key.clone(),
            })
        } else {
            Poll::Pending
        }
    }
}

impl Drop for SendLockFuture {
    fn drop(&mut self) {
        let Some(waiter_id) = self.waiter_id else {
            return;
        };
        let wake = SEND_LOCKS.with(|locks| {
            let mut locks = locks.borrow_mut();
            let Some(state) = locks.get_mut(&self.key) else {
                return None;
            };
            let was_first = state.waiters.front().map(|waiter| waiter.id) == Some(waiter_id);
            state.waiters.retain(|waiter| waiter.id != waiter_id);
            let wake = if was_first && !state.held {
                state.waiters.front().map(|waiter| waiter.waker.clone())
            } else {
                None
            };
            if !state.held && state.waiters.is_empty() {
                locks.remove(&self.key);
            }
            wake
        });
        if let Some(waker) = wake {
            waker.wake();
        }
    }
}

impl Drop for SendLockGuard {
    fn drop(&mut self) {
        let wake = SEND_LOCKS.with(|locks| {
            let mut locks = locks.borrow_mut();
            let Some(state) = locks.get_mut(&self.key) else {
                return None;
            };
            state.held = false;
            let wake = state.waiters.front().map(|waiter| waiter.waker.clone());
            if state.waiters.is_empty() {
                locks.remove(&self.key);
            }
            wake
        });
        if let Some(waker) = wake {
            waker.wake();
        }
    }
}

/// Forget outbound sequence counters for a host. Called on disconnect so that
/// a subsequent reconnect starts each stream at seq=0 again, which is what
/// the server's freshly-constructed `SeqValidator` expects.
pub fn clear_host_seqs(host_id: &str) {
    HOST_EPOCHS.with(|epochs| {
        let mut epochs = epochs.borrow_mut();
        let epoch = epochs.entry(host_id.to_owned()).or_insert(0);
        *epoch = epoch.wrapping_add(1);
    });
    SEQ_MAP.with(|map| {
        map.borrow_mut().retain(|(h, _), _| h != host_id);
    });
}

pub async fn send_frame<T: Serialize>(
    host_id: &str,
    stream: StreamPath,
    kind: FrameKind,
    payload: &T,
) -> Result<(), String> {
    let _send_lock = acquire_send_lock(host_id, &stream).await;
    let reservation = reserve_seq(host_id, &stream);
    let seq = reservation.seq;
    log::info!(
        "host_frame_tx host={} stream={} seq={} kind={}",
        host_id,
        stream,
        seq,
        kind
    );
    let envelope = match Envelope::from_payload(stream.clone(), kind, seq, payload) {
        Ok(envelope) => envelope,
        Err(error) => {
            release_seq_if_last(host_id, &stream, reservation);
            return Err(error.to_string());
        }
    };
    let line = match serde_json::to_string(&envelope) {
        Ok(line) => line,
        Err(error) => {
            release_seq_if_last(host_id, &stream, reservation);
            return Err(error.to_string());
        }
    };
    match bridge::send_host_line(bridge::SendHostLineRequest {
        host_id: host_id.to_owned(),
        line,
    })
    .await
    {
        Ok(()) => Ok(()),
        Err(e) => {
            release_seq_if_last(host_id, &stream, reservation);
            log::error!(
                "host_frame_tx_err host={} stream={} seq={} kind={} error={}",
                host_id,
                stream,
                seq,
                kind,
                e
            );
            Err(e)
        }
    }
}

/// Send an Agents-view preference mutation to the primary local host. The
/// server persists it and fans out a full `AgentsViewPreferencesNotify`
/// snapshot, which reconciles the optimistic overlay installed by the caller.
/// Routed on the host stream like settings/project mutations.
pub async fn set_agents_view_preferences(
    host_id: &str,
    host_stream: StreamPath,
    update: AgentsViewPreferencesUpdate,
) -> Result<(), String> {
    send_frame(
        host_id,
        host_stream,
        FrameKind::SetAgentsViewPreferences,
        &SetAgentsViewPreferencesPayload { update },
    )
    .await
}

/// Send a Smart View mutation to the primary local host. Like
/// `set_agents_view_preferences`, the server persists it and fans out a full
/// `AgentsViewPreferencesNotify` snapshot (which now carries `smart_views`),
/// reconciling any optimistic overlay installed by the caller. `SetActive` is a
/// server-side compound mutation: it sets the active view id and copies that
/// view's query into the active preferences, all in one authoritative snapshot.
/// Routed on the host stream like the preference mutations.
pub async fn set_agents_smart_views(
    host_id: &str,
    host_stream: StreamPath,
    update: AgentsSmartViewsUpdate,
) -> Result<(), String> {
    send_frame(
        host_id,
        host_stream,
        FrameKind::SetAgentsSmartViews,
        &SetAgentsSmartViewsPayload { update },
    )
    .await
}

/// Send a manual-tag mutation to the primary local host. Like the preference
/// and Smart View frames, the server persists it and fans out a full
/// `AgentsViewPreferencesNotify` snapshot (which carries the updated `tags`),
/// so tag chips and the tag picker re-render purely from the new snapshot.
/// Routed on the host stream like the other Agents-view mutations.
pub async fn set_agent_tags(
    host_id: &str,
    host_stream: StreamPath,
    update: AgentTagsUpdate,
) -> Result<(), String> {
    send_frame(
        host_id,
        host_stream,
        FrameKind::SetAgentTags,
        &SetAgentTagsPayload { update },
    )
    .await
}

/// Send a pin/unpin mutation to the primary local host. The server canonicalizes
/// pinned targets (session-keyed where possible) and fans out a full
/// `AgentsViewPreferencesNotify` snapshot carrying the updated `pins`, which the
/// Pinned section renders from. Routed on the host stream.
pub async fn set_agent_pins(
    host_id: &str,
    host_stream: StreamPath,
    update: AgentPinsUpdate,
) -> Result<(), String> {
    send_frame(
        host_id,
        host_stream,
        FrameKind::SetAgentPins,
        &SetAgentPinsPayload { update },
    )
    .await
}

/// Send a custom group mutation to the primary local host. Groups are
/// server-owned and carried by the full `AgentsViewPreferencesNotify`
/// snapshot, so callers should treat the sent frame as interaction state and
/// render from the next authoritative snapshot.
pub async fn set_agent_groups(
    host_id: &str,
    host_stream: StreamPath,
    update: AgentGroupsUpdate,
) -> Result<(), String> {
    send_frame(
        host_id,
        host_stream,
        FrameKind::SetAgentGroups,
        &SetAgentGroupsPayload { update },
    )
    .await
}

/// Tell the host the user opened/selected a project so the server can warm its
/// code intelligence and restore recent history for that project. Routed on the
/// project stream (`/project/<project_id>`). The server owns idempotency, so
/// re-sending on re-selection is harmless.
#[cfg(target_arch = "wasm32")]
pub async fn project_accessed(host_id: &str, project_stream: StreamPath) -> Result<(), String> {
    send_frame(
        host_id,
        project_stream,
        FrameKind::ProjectAccessed,
        &protocol::ProjectAccessedPayload::default(),
    )
    .await
}

pub async fn close_agent(host_id: &str, agent_stream: StreamPath) -> Result<(), String> {
    send_frame(
        host_id,
        agent_stream,
        FrameKind::CloseAgent,
        &CloseAgentPayload {},
    )
    .await
}

/// Fire a compaction request for the agent reached via `agent_stream`.
/// The server parses the agent id from the stream path; the payload only
/// carries optional tuning fields. Mirrors `close_agent`'s targeting
/// pattern.
pub async fn compact_agent(host_id: &str, agent_stream: StreamPath) -> Result<(), String> {
    send_frame(
        host_id,
        agent_stream,
        FrameKind::AgentCompact,
        &AgentCompactPayload::default(),
    )
    .await
}

/// Fire a team-wide compaction request. The server fans out per-member
/// compactions and emits `TeamCompactNotify` + per-agent
/// `AgentCompactNotify` events. Routed on the host stream because the
/// team itself is host-scoped (no per-team instance stream).
pub async fn team_compact(
    host_id: &str,
    host_stream: StreamPath,
    team_id: TeamId,
) -> Result<(), String> {
    send_frame(
        host_id,
        host_stream,
        FrameKind::TeamCompact,
        &TeamCompactPayload {
            team_id,
            summary_prompt: None,
            max_summary_bytes: None,
        },
    )
    .await
}

/// Ask the server to mint a fresh mobile pairing offer. The server
/// replies on the host stream with `MobilePairingOffer` (carrying the
/// `qr_uri`) and an updated `MobileAccessState` snapshot whose
/// pairing phase transitions to `Active`.
pub async fn mobile_pairing_start(host_id: &str, host_stream: StreamPath) -> Result<(), String> {
    send_frame(
        host_id,
        host_stream,
        FrameKind::MobilePairingStart,
        &MobilePairingStartPayload {},
    )
    .await
}

/// Cancel an in-flight pairing offer. The server confirms by pushing
/// a fresh `MobileAccessState` with `pairing.kind == Cancelled` and
/// drops the active offer so the QR stops being honoured.
pub async fn mobile_pairing_cancel(
    host_id: &str,
    host_stream: StreamPath,
    offer_id: MobilePairingOfferId,
) -> Result<(), String> {
    send_frame(
        host_id,
        host_stream,
        FrameKind::MobilePairingCancel,
        &MobilePairingCancelPayload { offer_id },
    )
    .await
}

/// Remove a previously paired mobile device from the host-side pairing store.
/// The server replies by broadcasting a fresh `MobileAccessState`.
pub async fn mobile_device_revoke(
    host_id: &str,
    host_stream: StreamPath,
    device_id: MobileDeviceId,
) -> Result<(), String> {
    send_frame(
        host_id,
        host_stream,
        FrameKind::MobileDeviceRevoke,
        &MobileDeviceRevokePayload { device_id },
    )
    .await
}

pub async fn custom_agent_upsert(
    host_id: &str,
    host_stream: StreamPath,
    custom_agent: CustomAgent,
) -> Result<(), String> {
    send_frame(
        host_id,
        host_stream,
        FrameKind::CustomAgentUpsert,
        &CustomAgentUpsertPayload { custom_agent },
    )
    .await
}

pub async fn custom_agent_delete(
    host_id: &str,
    host_stream: StreamPath,
    id: CustomAgentId,
) -> Result<(), String> {
    send_frame(
        host_id,
        host_stream,
        FrameKind::CustomAgentDelete,
        &CustomAgentDeletePayload { id },
    )
    .await
}

pub async fn mcp_server_upsert(
    host_id: &str,
    host_stream: StreamPath,
    mcp_server: McpServerConfig,
) -> Result<(), String> {
    send_frame(
        host_id,
        host_stream,
        FrameKind::McpServerUpsert,
        &McpServerUpsertPayload { mcp_server },
    )
    .await
}

pub async fn mcp_server_delete(
    host_id: &str,
    host_stream: StreamPath,
    id: McpServerId,
) -> Result<(), String> {
    send_frame(
        host_id,
        host_stream,
        FrameKind::McpServerDelete,
        &McpServerDeletePayload { id },
    )
    .await
}

pub async fn steering_upsert(
    host_id: &str,
    host_stream: StreamPath,
    steering: Steering,
) -> Result<(), String> {
    send_frame(
        host_id,
        host_stream,
        FrameKind::SteeringUpsert,
        &SteeringUpsertPayload { steering },
    )
    .await
}

pub async fn steering_delete(
    host_id: &str,
    host_stream: StreamPath,
    id: SteeringId,
) -> Result<(), String> {
    send_frame(
        host_id,
        host_stream,
        FrameKind::SteeringDelete,
        &SteeringDeletePayload { id },
    )
    .await
}

pub async fn skill_refresh(host_id: &str, host_stream: StreamPath) -> Result<(), String> {
    send_frame(
        host_id,
        host_stream,
        FrameKind::SkillRefresh,
        &SkillRefreshPayload {},
    )
    .await
}

pub async fn workflow_refresh(host_id: &str, host_stream: StreamPath) -> Result<(), String> {
    send_frame(
        host_id,
        host_stream,
        FrameKind::WorkflowRefresh,
        &WorkflowRefreshPayload::default(),
    )
    .await
}

pub async fn trigger_workflow(
    host_id: &str,
    host_stream: StreamPath,
    workflow_id: WorkflowId,
    project_id: Option<ProjectId>,
    inputs: HashMap<String, Value>,
) -> Result<(), String> {
    send_frame(
        host_id,
        host_stream,
        FrameKind::TriggerWorkflow,
        &TriggerWorkflowPayload {
            workflow_id,
            project_id,
            inputs,
        },
    )
    .await
}

pub async fn cancel_workflow(
    host_id: &str,
    host_stream: StreamPath,
    run_id: WorkflowRunId,
) -> Result<(), String> {
    send_frame(
        host_id,
        host_stream,
        FrameKind::CancelWorkflow,
        &CancelWorkflowPayload { run_id },
    )
    .await
}

pub async fn team_delete(host_id: &str, host_stream: StreamPath, id: TeamId) -> Result<(), String> {
    send_frame(
        host_id,
        host_stream,
        FrameKind::TeamDelete,
        &TeamDeletePayload { id },
    )
    .await
}

pub async fn team_set_manager(
    host_id: &str,
    host_stream: StreamPath,
    team_id: TeamId,
    new_manager_member_id: TeamMemberId,
) -> Result<(), String> {
    send_frame(
        host_id,
        host_stream,
        FrameKind::TeamSetManager,
        &TeamSetManagerPayload {
            team_id,
            new_manager_member_id,
        },
    )
    .await
}

pub async fn team_member_create(
    host_id: &str,
    host_stream: StreamPath,
    team_id: TeamId,
    member: TeamMemberCreateSpec,
) -> Result<(), String> {
    send_frame(
        host_id,
        host_stream,
        FrameKind::TeamMemberCreate,
        &TeamMemberCreatePayload {
            team_id,
            member,
            session_id: None,
        },
    )
    .await
}

pub async fn team_member_update(
    host_id: &str,
    host_stream: StreamPath,
    payload: TeamMemberUpdatePayload,
) -> Result<(), String> {
    send_frame(host_id, host_stream, FrameKind::TeamMemberUpdate, &payload).await
}

pub async fn team_member_delete(
    host_id: &str,
    host_stream: StreamPath,
    id: TeamMemberId,
) -> Result<(), String> {
    send_frame(
        host_id,
        host_stream,
        FrameKind::TeamMemberDelete,
        &TeamMemberDeletePayload { id },
    )
    .await
}

pub async fn team_member_activate(
    host_id: &str,
    host_stream: StreamPath,
    member_id: TeamMemberId,
    prompt: Option<String>,
    images: Option<Vec<ImageData>>,
) -> Result<(), String> {
    send_frame(
        host_id,
        host_stream,
        FrameKind::TeamMemberActivate,
        &TeamMemberActivatePayload {
            member_id,
            prompt,
            images,
        },
    )
    .await
}

pub async fn team_draft_create(
    host_id: &str,
    host_stream: StreamPath,
    template_id: Option<TeamTemplateId>,
) -> Result<(), String> {
    send_frame(
        host_id,
        host_stream,
        FrameKind::TeamDraftCreate,
        &TeamDraftCreatePayload { template_id },
    )
    .await
}

pub async fn team_draft_set_name(
    host_id: &str,
    host_stream: StreamPath,
    draft_id: TeamDraftId,
    name: String,
) -> Result<(), String> {
    send_frame(
        host_id,
        host_stream,
        FrameKind::TeamDraftUpdate,
        &TeamDraftUpdatePayload::SetName { draft_id, name },
    )
    .await
}

pub async fn team_draft_replace_member(
    host_id: &str,
    host_stream: StreamPath,
    draft_id: TeamDraftId,
    member: TeamDraftMemberEdit,
) -> Result<(), String> {
    send_frame(
        host_id,
        host_stream,
        FrameKind::TeamDraftUpdate,
        &TeamDraftUpdatePayload::ReplaceMember { draft_id, member },
    )
    .await
}

pub async fn team_draft_add_report(
    host_id: &str,
    host_stream: StreamPath,
    draft_id: TeamDraftId,
) -> Result<(), String> {
    send_frame(
        host_id,
        host_stream,
        FrameKind::TeamDraftUpdate,
        &TeamDraftUpdatePayload::AddReport { draft_id },
    )
    .await
}

pub async fn team_draft_remove_member(
    host_id: &str,
    host_stream: StreamPath,
    draft_id: TeamDraftId,
    member_id: TeamDraftMemberId,
) -> Result<(), String> {
    send_frame(
        host_id,
        host_stream,
        FrameKind::TeamDraftUpdate,
        &TeamDraftUpdatePayload::RemoveMember {
            draft_id,
            member_id,
        },
    )
    .await
}

pub async fn team_draft_set_member_profile(
    host_id: &str,
    host_stream: StreamPath,
    payload: TeamDraftUpdatePayload,
) -> Result<(), String> {
    send_frame(host_id, host_stream, FrameKind::TeamDraftUpdate, &payload).await
}

pub async fn team_draft_shuffle(
    host_id: &str,
    host_stream: StreamPath,
    draft_id: TeamDraftId,
    member_id: Option<TeamDraftMemberId>,
    scope: TeamDraftShuffleScope,
) -> Result<(), String> {
    send_frame(
        host_id,
        host_stream,
        FrameKind::TeamDraftShuffle,
        &TeamDraftShufflePayload {
            draft_id,
            member_id,
            scope,
        },
    )
    .await
}

pub async fn team_member_shuffle(
    host_id: &str,
    host_stream: StreamPath,
    team_id: TeamId,
) -> Result<(), String> {
    send_frame(
        host_id,
        host_stream,
        FrameKind::TeamMemberShuffle,
        &TeamMemberShufflePayload { team_id },
    )
    .await
}

pub async fn team_draft_apply_template(
    host_id: &str,
    host_stream: StreamPath,
    draft_id: TeamDraftId,
    template_id: TeamTemplateId,
) -> Result<(), String> {
    send_frame(
        host_id,
        host_stream,
        FrameKind::TeamDraftApplyTemplate,
        &TeamDraftApplyTemplatePayload {
            draft_id,
            template_id,
        },
    )
    .await
}

pub async fn team_draft_commit(
    host_id: &str,
    host_stream: StreamPath,
    draft_id: TeamDraftId,
) -> Result<(), String> {
    send_frame(
        host_id,
        host_stream,
        FrameKind::TeamDraftCommit,
        &TeamDraftCommitPayload { draft_id },
    )
    .await
}

pub async fn team_draft_discard(
    host_id: &str,
    host_stream: StreamPath,
    draft_id: TeamDraftId,
) -> Result<(), String> {
    send_frame(
        host_id,
        host_stream,
        FrameKind::TeamDraftDiscard,
        &TeamDraftDiscardPayload { draft_id },
    )
    .await
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use std::rc::Rc;

    async fn settle() {
        for _ in 0..2 {
            let promise = js_sys::Promise::new(&mut |resolve, _| {
                web_sys::window()
                    .expect("window")
                    .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 0)
                    .expect("schedule test turn");
            });
            let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
        }
    }

    fn install_reject_once_send_stub() {
        js_sys::eval(
            r#"
            (function() {
                window.__test_send_lines = [];
                window.__test_reject_sends = 1;
                window.__TAURI__ = window.__TAURI__ || {};
                window.__TAURI__.core = window.__TAURI__.core || {};
                window.__TAURI__.core.invoke = function(cmd, args) {
                    if (cmd !== "send_host_line") {
                        return Promise.resolve();
                    }
                    window.__test_send_lines.push(args.line);
                    if (window.__test_reject_sends > 0) {
                        window.__test_reject_sends -= 1;
                        return Promise.reject("simulated bridge rejection");
                    }
                    return Promise.resolve();
                };
            })();
            "#,
        )
        .expect("install reject-once send stub");
    }

    fn install_deferred_send_stub() {
        js_sys::eval(
            r#"
            (function() {
                window.__test_send_lines = [];
                window.__test_send_resolvers = [];
                window.__TAURI__ = window.__TAURI__ || {};
                window.__TAURI__.core = window.__TAURI__.core || {};
                window.__TAURI__.core.invoke = function(cmd, args) {
                    if (cmd !== "send_host_line") {
                        return Promise.resolve();
                    }
                    window.__test_send_lines.push(args.line);
                    return new Promise(function(resolve) {
                        window.__test_send_resolvers.push(resolve);
                    });
                };
            })();
            "#,
        )
        .expect("install deferred send stub");
    }

    fn resolve_next_send() {
        js_sys::eval(
            r#"
            (function() {
                const resolve = window.__test_send_resolvers.shift();
                if (!resolve) throw new Error("no deferred send");
                resolve();
            })();
            "#,
        )
        .expect("resolve next deferred send");
    }

    fn sent_envelopes() -> Vec<Envelope> {
        let raw = js_sys::eval("JSON.stringify(window.__test_send_lines || [])")
            .expect("read captured host lines")
            .as_string()
            .unwrap_or_else(|| "[]".to_owned());
        let lines: Vec<String> = serde_json::from_str(&raw).expect("captured lines are strings");
        lines
            .iter()
            .map(|line| serde_json::from_str(line).expect("captured line is an envelope"))
            .collect()
    }

    #[wasm_bindgen_test::wasm_bindgen_test]
    async fn rejected_send_does_not_consume_protocol_sequence() {
        let host_id = "host-sequence";
        let stream = StreamPath("/host/sequence".to_owned());
        clear_host_seqs(host_id);
        install_reject_once_send_stub();

        let rejected = send_frame(
            host_id,
            stream.clone(),
            FrameKind::ClientError,
            &serde_json::json!({"attempt": 1}),
        )
        .await;
        assert!(rejected.is_err());
        assert_eq!(current_seq(host_id, &stream), 0);

        send_frame(
            host_id,
            stream.clone(),
            FrameKind::ClientError,
            &serde_json::json!({"attempt": 2}),
        )
        .await
        .expect("the second bridge send succeeds");

        let envelopes = sent_envelopes();
        assert_eq!(envelopes.len(), 2);
        assert_eq!(envelopes[0].seq, 0);
        assert_eq!(envelopes[1].seq, 0);
        assert_eq!(current_seq(host_id, &stream), 1);
    }

    #[wasm_bindgen_test::wasm_bindgen_test]
    async fn concurrent_sends_keep_distinct_ordered_sequences() {
        let host_id = "host-concurrent";
        let stream = StreamPath("/host/concurrent".to_owned());
        clear_host_seqs(host_id);
        install_deferred_send_stub();

        let outcomes = Rc::new(RefCell::new(Vec::new()));
        for attempt in 1..=2 {
            let outcomes = Rc::clone(&outcomes);
            let stream = stream.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let result = send_frame(
                    host_id,
                    stream,
                    FrameKind::ClientError,
                    &serde_json::json!({"attempt": attempt}),
                )
                .await;
                outcomes.borrow_mut().push(result);
            });
        }

        settle().await;
        let envelopes = sent_envelopes();
        assert_eq!(envelopes.len(), 1);
        assert_eq!(envelopes[0].seq, 0);
        assert_eq!(current_seq(host_id, &stream), 1);

        resolve_next_send();
        settle().await;
        let envelopes = sent_envelopes();
        assert_eq!(envelopes.len(), 2);
        assert_eq!(envelopes[0].seq, 0);
        assert_eq!(envelopes[1].seq, 1);
        assert_eq!(current_seq(host_id, &stream), 2);

        resolve_next_send();
        settle().await;
        let outcomes = outcomes.borrow();
        assert_eq!(outcomes.len(), 2);
        assert!(outcomes.iter().all(Result::is_ok));
    }

    #[wasm_bindgen_test::wasm_bindgen_test]
    async fn clearing_a_host_does_not_restore_a_suspended_reservation() {
        let host_id = "host-clear";
        let stream = StreamPath("/host/clear".to_owned());
        clear_host_seqs(host_id);
        install_deferred_send_stub();

        let outcome = Rc::new(RefCell::new(None));
        let captured_outcome = Rc::clone(&outcome);
        let suspended_stream = stream.clone();
        wasm_bindgen_futures::spawn_local(async move {
            *captured_outcome.borrow_mut() = Some(
                send_frame(
                    host_id,
                    suspended_stream,
                    FrameKind::ClientError,
                    &serde_json::json!({"attempt": 1}),
                )
                .await,
            );
        });

        settle().await;
        assert_eq!(sent_envelopes().len(), 1);
        assert_eq!(current_seq(host_id, &stream), 1);

        clear_host_seqs(host_id);
        assert_eq!(current_seq(host_id, &stream), 0);
        resolve_next_send();
        settle().await;

        assert!(matches!(&*outcome.borrow(), Some(Ok(()))));
        assert_eq!(current_seq(host_id, &stream), 0);
    }
}
