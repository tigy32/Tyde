use std::cell::RefCell;
use std::collections::HashMap;

use protocol::types::{AgentCompactPayload, TeamCompactPayload};
use protocol::{
    AgentPinsUpdate, AgentTagsUpdate, AgentsSmartViewsUpdate, AgentsViewPreferencesUpdate,
    CancelWorkflowPayload, CloseAgentPayload, CustomAgent, CustomAgentDeletePayload, CustomAgentId,
    CustomAgentUpsertPayload, Envelope, FrameKind, ImageData, McpServerConfig,
    McpServerDeletePayload, McpServerId, McpServerUpsertPayload, MobileDeviceId,
    MobileDeviceRevokePayload, MobilePairingCancelPayload, MobilePairingOfferId,
    MobilePairingStartPayload, ProjectId, SetAgentPinsPayload, SetAgentTagsPayload,
    SetAgentsSmartViewsPayload, SetAgentsViewPreferencesPayload, SkillRefreshPayload, Steering,
    SteeringDeletePayload, SteeringId, SteeringUpsertPayload, StreamPath, TeamDeletePayload,
    TeamDraftApplyTemplatePayload, TeamDraftCommitPayload, TeamDraftCreatePayload,
    TeamDraftDiscardPayload, TeamDraftId, TeamDraftMemberEdit, TeamDraftMemberId,
    TeamDraftShufflePayload, TeamDraftShuffleScope, TeamDraftUpdatePayload, TeamId,
    TeamMemberActivatePayload, TeamMemberCreatePayload, TeamMemberCreateSpec,
    TeamMemberDeletePayload, TeamMemberId, TeamMemberShufflePayload, TeamMemberUpdatePayload,
    TeamSetManagerPayload, TeamTemplateId, TriggerWorkflowPayload, WorkflowId,
    WorkflowRefreshPayload, WorkflowRunId,
};
use serde::Serialize;
use serde_json::Value;

use crate::bridge;

// WASM is single-threaded, so RefCell is fine.
// Per-stream monotonic sequence numbers, as required by the protocol.
thread_local! {
    static SEQ_MAP: RefCell<HashMap<(String, StreamPath), u64>> = RefCell::new(HashMap::new());
}

fn next_seq(host_id: &str, stream: &StreamPath) -> u64 {
    SEQ_MAP.with(|map| {
        let mut map = map.borrow_mut();
        let counter = map.entry((host_id.to_owned(), stream.clone())).or_insert(0);
        let v = *counter;
        *counter += 1;
        v
    })
}

/// Forget outbound sequence counters for a host. Called on disconnect so that
/// a subsequent reconnect starts each stream at seq=0 again, which is what
/// the server's freshly-constructed `SeqValidator` expects.
pub fn clear_host_seqs(host_id: &str) {
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
    let seq = next_seq(host_id, &stream);
    log::info!(
        "host_frame_tx host={} stream={} seq={} kind={}",
        host_id,
        stream,
        seq,
        kind
    );
    let envelope =
        Envelope::from_payload(stream.clone(), kind, seq, payload).map_err(|e| e.to_string())?;
    let line = serde_json::to_string(&envelope).map_err(|e| e.to_string())?;
    match bridge::send_host_line(bridge::SendHostLineRequest {
        host_id: host_id.to_owned(),
        line,
    })
    .await
    {
        Ok(()) => Ok(()),
        Err(e) => {
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
