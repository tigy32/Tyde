use std::cell::RefCell;
use std::collections::HashMap;

use protocol::{
    CloseAgentPayload, CustomAgent, CustomAgentDeletePayload, CustomAgentId,
    CustomAgentUpsertPayload, Envelope, FrameKind, ImageData, McpServerConfig,
    McpServerDeletePayload, McpServerId, McpServerUpsertPayload, SkillRefreshPayload, Steering,
    SteeringDeletePayload, SteeringId, SteeringUpsertPayload, StreamPath, TeamCreatePayload,
    TeamDeletePayload, TeamId, TeamMemberActivatePayload, TeamMemberCreatePayload,
    TeamMemberCreateSpec, TeamMemberDeletePayload, TeamMemberId, TeamMemberUpdatePayload,
    TeamSetManagerPayload,
};
use serde::Serialize;

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
    let envelope = Envelope::from_payload(stream, kind, seq, payload).map_err(|e| e.to_string())?;
    let line = serde_json::to_string(&envelope).map_err(|e| e.to_string())?;
    bridge::send_host_line(bridge::SendHostLineRequest {
        host_id: host_id.to_owned(),
        line,
    })
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

pub async fn team_create(
    host_id: &str,
    host_stream: StreamPath,
    name: String,
    manager: TeamMemberCreateSpec,
) -> Result<(), String> {
    send_frame(
        host_id,
        host_stream,
        FrameKind::TeamCreate,
        &TeamCreatePayload { name, manager },
    )
    .await
}

pub async fn team_delete(
    host_id: &str,
    host_stream: StreamPath,
    id: TeamId,
) -> Result<(), String> {
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
