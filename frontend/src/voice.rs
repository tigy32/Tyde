use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
};

use leptos::prelude::*;
use wasm_bindgen_futures::spawn_local;

use crate::{
    bridge,
    send::{send_binary_frame, send_frame},
    state::{ActiveAgentRef, AgentInfo, AppState},
};
use protocol::{FrameKind, StreamPath};

struct PreparedVoiceUplink {
    host_id: String,
    stream: StreamPath,
    payload: protocol::VoiceAudioPayload,
    opus: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq)]
enum VoiceUplinkRejection {
    Inactive,
    Generation,
}

fn prepare_voice_uplink(
    state: &VoiceUiState,
    event: bridge::VoiceOpusPacketEvent,
) -> Result<PreparedVoiceUplink, VoiceUplinkRejection> {
    let VoiceUiState::Active {
        generation,
        host_id,
        session_id,
        ..
    } = state
    else {
        return Err(VoiceUplinkRejection::Inactive);
    };
    if *generation != event.generation {
        return Err(VoiceUplinkRejection::Generation);
    }
    Ok(PreparedVoiceUplink {
        host_id: host_id.clone(),
        stream: StreamPath(format!("/voice/{}", session_id.0)),
        payload: protocol::VoiceAudioPayload {
            session_id: session_id.clone(),
            generation: *generation,
            direction: protocol::VoiceDirection::Input,
            first_media_seq: event.media_seq,
            timestamp_samples_48k: event.timestamp_samples_48k,
            packet_lengths: vec![event.opus.len() as u16],
        },
        opus: event.opus,
    })
}

async fn send_prepared_voice_uplink_with<F, Fut>(
    prepared: PreparedVoiceUplink,
    send: F,
) -> Result<(), String>
where
    F: FnOnce(String, StreamPath, protocol::VoiceAudioPayload, Vec<u8>) -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    send(
        prepared.host_id,
        prepared.stream,
        prepared.payload,
        prepared.opus,
    )
    .await
}

#[derive(Clone, Debug, PartialEq)]
pub enum VoiceUiState {
    Idle,
    Starting {
        generation: u64,
        host_id: String,
        target: protocol::VoiceTarget,
    },
    Active {
        generation: u64,
        host_id: String,
        session_id: protocol::VoiceSessionId,
        target: protocol::VoiceTarget,
        state: protocol::VoiceSessionState,
        transcript: Option<protocol::VoiceTranscriptPayload>,
        next_output_media_seq: u64,
        dropped_output_packets: u64,
    },
    Failed(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TargetResolutionState {
    target_resolvable: bool,
    #[cfg(test)]
    reason: &'static str,
}

fn target_resolution_state(
    active_agent_present: bool,
    matching_agent_present: bool,
    agent_started: bool,
    agent_fatal_error: bool,
) -> TargetResolutionState {
    let reason = if !active_agent_present {
        "active_agent_missing"
    } else if !matching_agent_present {
        "matching_agent_missing"
    } else if !agent_started {
        "not_started"
    } else if agent_fatal_error {
        "fatal_error"
    } else {
        "resolved"
    };
    TargetResolutionState {
        target_resolvable: reason == "resolved",
        #[cfg(test)]
        reason,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct VoiceGateState {
    gate_available: bool,
}

fn voice_gate_state(
    target: TargetResolutionState,
    voice_enabled: bool,
    nova_available: bool,
) -> VoiceGateState {
    let gate_available = target.target_resolvable && voice_enabled && nova_available;
    VoiceGateState { gate_available }
}

fn resolve_target(
    active_agent: Option<ActiveAgentRef>,
    agents: &[AgentInfo],
) -> (
    TargetResolutionState,
    Option<(String, protocol::VoiceTarget)>,
) {
    let Some(ActiveAgentRef { host_id, agent_id }) = active_agent else {
        return (target_resolution_state(false, false, false, false), None);
    };
    let match_state = if let Some(agent) = agents.iter().find(|agent| {
        agent.host_id == host_id
            && agent.agent_id == agent_id
            && agent.started
            && agent.fatal_error.is_none()
    }) {
        Some((true, false, Some(agent.instance_stream.clone())))
    } else {
        agents
            .iter()
            .find(|agent| agent.host_id == host_id && agent.agent_id == agent_id)
            .map(|agent| (agent.started, agent.fatal_error.is_some(), None))
    };
    let Some((started, fatal_error, instance_stream)) = match_state else {
        return (target_resolution_state(true, false, false, false), None);
    };
    let resolution = target_resolution_state(true, true, started, fatal_error);
    let target = instance_stream.map(|instance_stream| {
        (
            host_id,
            protocol::VoiceTarget {
                agent_id,
                instance_stream,
            },
        )
    });
    (resolution, target)
}

fn target_with_resolution(
    state: &AppState,
) -> (
    TargetResolutionState,
    Option<(String, protocol::VoiceTarget)>,
) {
    let active_agent = state.active_agent.get_untracked();
    state
        .agents
        .with_untracked(|agents| resolve_target(active_agent, agents))
}

fn target_with_resolution_tracked(
    state: &AppState,
) -> (
    TargetResolutionState,
    Option<(String, protocol::VoiceTarget)>,
) {
    let active_agent = state.active_agent.get();
    state
        .agents
        .with(|agents| resolve_target(active_agent, agents))
}

fn target(state: &AppState) -> Option<(String, protocol::VoiceTarget)> {
    target_with_resolution(state).1
}

pub fn start(state: AppState) {
    let Some((host_id, target)) = target(&state) else {
        return;
    };
    let generation = state
        .voice_generation
        .try_update(|value| {
            *value = value.saturating_add(1);
            *value
        })
        .unwrap_or(1);
    state.voice_ui.set(VoiceUiState::Starting {
        generation,
        host_id: host_id.clone(),
        target: target.clone(),
    });
    spawn_local(async move {
        let payload = protocol::VoiceStartPayload {
            generation,
            target,
            formats: vec![protocol::VoiceFormatPair {
                uplink: protocol::VoiceAudioFormat::opus(48_000),
                downlink: protocol::VoiceAudioFormat::opus(24_000),
            }],
        };
        if let Err(error) = send_frame(
            &host_id,
            StreamPath("/voice".into()),
            FrameKind::VoiceStart,
            &payload,
        )
        .await
        {
            state.voice_ui.set(VoiceUiState::Failed(error));
        }
    });
}

pub fn stop(state: AppState, reason: protocol::VoiceStopReason) {
    let current = state.voice_ui.get_untracked();
    let VoiceUiState::Active {
        generation,
        host_id,
        session_id,
        dropped_output_packets,
        ..
    } = current
    else {
        state.voice_ui.set(VoiceUiState::Idle);
        return;
    };
    state.voice_ui.set(VoiceUiState::Idle);
    spawn_local(async move {
        let _ = bridge::voice_media_stop().await;
        let stats = protocol::VoiceFlowStats {
            dropped_packets: dropped_output_packets,
            ..Default::default()
        };
        let payload = protocol::VoiceStopPayload {
            session_id: session_id.clone(),
            generation,
            reason,
            stats,
        };
        let _ = send_frame(
            &host_id,
            StreamPath(format!("/voice/{}", session_id.0)),
            FrameKind::VoiceStop,
            &payload,
        )
        .await;
    });
}

pub fn interrupt(state: AppState) {
    let VoiceUiState::Active {
        generation,
        host_id,
        session_id,
        ..
    } = state.voice_ui.get_untracked()
    else {
        return;
    };
    spawn_local(async move {
        let _ = bridge::voice_media_flush_output(generation).await;
        let payload = protocol::VoiceSessionPayload {
            session_id: session_id.clone(),
            generation,
        };
        let _ = send_frame(
            &host_id,
            StreamPath(format!("/voice/{}", session_id.0)),
            FrameKind::VoiceInterrupt,
            &payload,
        )
        .await;
    });
}

pub fn handle_control(state: &AppState, host_id: &str, envelope: &protocol::Envelope) {
    match envelope.kind {
        FrameKind::VoiceAccepted => {
            if let Ok(payload) = envelope.parse_payload::<protocol::VoiceAcceptedPayload>() {
                let valid = matches!(state.voice_ui.get_untracked(),VoiceUiState::Starting{generation,host_id:ref pending_host,target:ref pending_target} if generation==payload.generation && pending_host==host_id && pending_target==&payload.target)
                    && target(state).is_some_and(|(active_host, active_target)| {
                        active_host == host_id && active_target == payload.target
                    });
                if !valid {
                    let host = host_id.to_owned();
                    spawn_local(async move {
                        let stop = protocol::VoiceStopPayload {
                            session_id: payload.session_id.clone(),
                            generation: payload.generation,
                            reason: protocol::VoiceStopReason::TargetChanged,
                            stats: Default::default(),
                        };
                        let _ = send_frame(
                            &host,
                            StreamPath(format!("/voice/{}", payload.session_id.0)),
                            FrameKind::VoiceStop,
                            &stop,
                        )
                        .await;
                    });
                    return;
                }
                let generation = payload.generation;
                state.voice_ui.set(VoiceUiState::Active {
                    generation,
                    host_id: host_id.into(),
                    session_id: payload.session_id,
                    target: payload.target,
                    state: protocol::VoiceSessionState::Listening,
                    transcript: None,
                    next_output_media_seq: 0,
                    dropped_output_packets: 0,
                });
                let media_state = state.clone();
                let media_host = host_id.to_owned();
                spawn_local(async move {
                    if let Err(error) = bridge::voice_media_start(&media_host, generation).await {
                        stop(media_state.clone(), protocol::VoiceStopReason::MediaFailed);
                        media_state.voice_ui.set(VoiceUiState::Failed(error));
                    }
                });
            }
        }
        FrameKind::VoiceTranscript => {
            if let Ok(transcript) = envelope.parse_payload::<protocol::VoiceTranscriptPayload>() {
                let agent_id = match state.voice_ui.get_untracked() {
                    VoiceUiState::Active {
                        generation,
                        ref session_id,
                        ref target,
                        ..
                    } if generation == transcript.generation
                        && session_id == &transcript.session_id =>
                    {
                        Some(target.agent_id.clone())
                    }
                    _ => None,
                };
                if agent_id.is_none() {
                    return;
                }
                state.voice_ui.update(|current| {
                    if let VoiceUiState::Active {
                        transcript: slot, ..
                    } = current
                    {
                        *slot = Some(transcript.clone())
                    }
                });
                if transcript.is_final
                    && let Some(agent_id) = agent_id
                {
                    state.push_chat_entry(
                        agent_id,
                        crate::state::ChatMessageEntry {
                            message: protocol::ChatMessage {
                                message_id: transcript.message_id,
                                timestamp: crate::state::now_ms(),
                                sender: match transcript.speaker {
                                    protocol::VoiceTranscriptSpeaker::User => {
                                        protocol::MessageSender::User
                                    }
                                    protocol::VoiceTranscriptSpeaker::Assistant => {
                                        protocol::MessageSender::Assistant {
                                            agent: "Nova Sonic".into(),
                                        }
                                    }
                                    protocol::VoiceTranscriptSpeaker::Progress => {
                                        protocol::MessageSender::System
                                    }
                                },
                                content: transcript.text,
                                reasoning: None,
                                tool_calls: vec![],
                                model_info: None,
                                token_usage: None,
                                context_breakdown: None,
                                images: None,
                            },
                            tool_requests: vec![],
                        },
                    );
                }
            }
        }
        FrameKind::VoiceState => {
            if let Ok(payload) = envelope.parse_payload::<protocol::VoiceStatePayload>() {
                let mut flush_generation = None;
                state.voice_ui.update(|current| {
                    if let VoiceUiState::Active {
                        generation,
                        session_id,
                        state,
                        ..
                    } = current
                        && *generation == payload.generation
                        && *session_id == payload.session_id
                    {
                        *state = payload.state;
                        if payload.state == protocol::VoiceSessionState::Interrupting {
                            flush_generation = Some(payload.generation);
                        }
                    }
                });
                if let Some(generation) = flush_generation {
                    spawn_local(async move {
                        let _ = bridge::voice_media_flush_output(generation).await;
                    });
                }
            }
        }
        FrameKind::VoiceStop => {
            if let Ok(payload) = envelope.parse_payload::<protocol::VoiceStopPayload>()
                && matches!(state.voice_ui.get_untracked(),VoiceUiState::Active{generation,ref session_id,..} if generation==payload.generation&&session_id==&payload.session_id)
            {
                state.voice_ui.set(VoiceUiState::Idle);
                spawn_local(async {
                    let _ = bridge::voice_media_stop().await;
                });
            }
        }
        FrameKind::VoiceError => {
            if let Ok(payload) = envelope.parse_payload::<protocol::VoiceErrorPayload>() {
                let applies = match state.voice_ui.get_untracked() {
                    VoiceUiState::Starting { generation, .. } => {
                        payload.session_id.is_none() && generation == payload.generation
                    }
                    VoiceUiState::Active {
                        generation,
                        ref session_id,
                        ..
                    } => {
                        generation == payload.generation
                            && payload.session_id.as_ref() == Some(session_id)
                    }
                    _ => false,
                };
                if applies {
                    state.voice_ui.set(VoiceUiState::Idle);
                    spawn_local(async {
                        let _ = bridge::voice_media_stop().await;
                    });
                }
            }
        }
        _ => {}
    }
}

fn handle_voice_opus_packet(packet_state: &AppState, event: bridge::VoiceOpusPacketEvent) {
    let prepared = match prepare_voice_uplink(&packet_state.voice_ui.get_untracked(), event) {
        Ok(prepared) => prepared,
        Err(_) => return,
    };
    spawn_local(async move {
        let _ = send_prepared_voice_uplink_with(
            prepared,
            |host_id, stream, payload, opus| async move {
                send_binary_frame(&host_id, stream, FrameKind::VoiceAudio, &payload, &opus).await
            },
        )
        .await;
    });
}

async fn register_voice_opus_listener(state: AppState) -> Result<bridge::UnlistenHandle, String> {
    bridge::listen_voice_opus_packet(move |event| handle_voice_opus_packet(&state, event)).await
}

struct VoiceMediaListenerHandles {
    handles: Vec<bridge::UnlistenHandle>,
}

impl VoiceMediaListenerHandles {
    fn new() -> Self {
        Self {
            handles: Vec::with_capacity(4),
        }
    }

    fn push(&mut self, handle: bridge::UnlistenHandle) {
        self.handles.push(handle);
    }
}

impl Drop for VoiceMediaListenerHandles {
    fn drop(&mut self) {
        for handle in self.handles.drain(..) {
            handle.remove();
        }
    }
}

thread_local! {
    static VOICE_MEDIA_LISTENER_NEXT_TOKEN: Cell<u64> = const { Cell::new(0) };
    static VOICE_MEDIA_LISTENER_SLOTS: RefCell<HashMap<u64, Option<VoiceMediaListenerHandles>>> =
        RefCell::new(HashMap::new());
}

fn begin_media_listener_lifecycle() -> u64 {
    let token = VOICE_MEDIA_LISTENER_NEXT_TOKEN.with(|next| {
        let token = next.get().wrapping_add(1);
        next.set(token);
        token
    });
    let displaced = VOICE_MEDIA_LISTENER_SLOTS.with(|slots| slots.borrow_mut().insert(token, None));
    drop(displaced);
    token
}

fn retain_media_listener_handles(token: u64, handles: VoiceMediaListenerHandles) {
    let retained = VOICE_MEDIA_LISTENER_SLOTS.with(|slots| {
        let mut slots = slots.borrow_mut();
        match slots.get_mut(&token) {
            Some(slot) => Ok(slot.replace(handles)),
            None => Err(handles),
        }
    });
    match retained {
        Ok(previous) => drop(previous),
        Err(stale) => drop(stale),
    }
}

fn dispose_media_listener_lifecycle(token: u64) {
    let handles = VOICE_MEDIA_LISTENER_SLOTS.with(|slots| slots.borrow_mut().remove(&token));
    drop(handles);
}

#[derive(Debug, PartialEq, Eq)]
enum VoiceDownlinkRejection {
    Inactive,
    Generation,
    Sequence,
}

fn admit_voice_downlink(
    current: &mut VoiceUiState,
    payload: &protocol::VoiceAudioPayload,
) -> Result<u64, (u64, VoiceDownlinkRejection)> {
    match current {
        VoiceUiState::Active {
            generation,
            session_id,
            next_output_media_seq,
            dropped_output_packets,
            ..
        } if *generation == payload.generation && session_id.0 == payload.session_id.0 => {
            if payload.first_media_seq < *next_output_media_seq {
                return Err((*generation, VoiceDownlinkRejection::Sequence));
            }
            *dropped_output_packets = dropped_output_packets.saturating_add(
                payload
                    .first_media_seq
                    .saturating_sub(*next_output_media_seq),
            );
            *next_output_media_seq = payload.first_media_seq + payload.packet_lengths.len() as u64;
            Ok(*generation)
        }
        VoiceUiState::Active { generation, .. } => {
            Err((*generation, VoiceDownlinkRejection::Generation))
        }
        _ => Err((payload.generation, VoiceDownlinkRejection::Inactive)),
    }
}

fn handle_host_voice_frame(frame_state: &AppState, event: bridge::HostVoiceFrameEvent) {
    let Ok(envelope) = serde_json::from_str::<protocol::Envelope>(&event.envelope) else {
        return;
    };
    if envelope.kind != FrameKind::VoiceAudio {
        handle_control(frame_state, &event.host_id, &envelope);
        return;
    }
    let Ok(payload) = envelope.parse_payload::<protocol::VoiceAudioPayload>() else {
        return;
    };
    let generation = payload.generation;
    let declared_bytes = payload.packet_lengths.iter().fold(0_usize, |total, len| {
        total.saturating_add(usize::from(*len))
    });
    let malformed_bounds = payload.packet_lengths.is_empty()
        || payload.packet_lengths.len() > protocol::MAX_VOICE_PACKETS_PER_FRAME
        || event.opus.len() > protocol::MAX_VOICE_AUDIO_BYTES
        || declared_bytes < event.opus.len();
    if malformed_bounds {
        return;
    }
    let mut admission = None;
    frame_state.voice_ui.update(|current| {
        admission = Some(admit_voice_downlink(current, &payload));
    });
    if admission.is_some_and(|result| result.is_err()) {
        return;
    }

    let mut offset = 0;
    for (packet_index, len) in payload.packet_lengths.iter().copied().enumerate() {
        let media_seq = payload.first_media_seq + packet_index as u64;
        let end = offset + len as usize;
        if end > event.opus.len() {
            return;
        }
        let packet = event.opus[offset..end].to_vec();
        let timestamp_samples_48k =
            payload.timestamp_samples_48k + (packet_index as u64).saturating_mul(960);
        spawn_local(async move {
            let _ = bridge::voice_media_push_output(
                generation,
                media_seq,
                timestamp_samples_48k,
                &packet,
            )
            .await;
        });
        offset = end;
    }
}

async fn register_media_listeners(state: AppState) -> Result<VoiceMediaListenerHandles, String> {
    let mut listeners = VoiceMediaListenerHandles::new();
    listeners.push(register_voice_opus_listener(state.clone()).await?);

    let frame_state = state.clone();
    listeners.push(
        bridge::listen_host_voice_frame(move |event| handle_host_voice_frame(&frame_state, event))
            .await?,
    );

    let disconnect_state = state.clone();
    listeners.push(
        bridge::listen_host_disconnected(move |event| {
            if matches!(
                disconnect_state.voice_ui.get_untracked(),
                VoiceUiState::Starting { ref host_id, .. }
                    | VoiceUiState::Active { ref host_id, .. }
                    if host_id == &event.host_id
            ) {
                stop(
                    disconnect_state.clone(),
                    protocol::VoiceStopReason::TransportLost,
                );
            }
        })
        .await?,
    );

    let media_state = state.clone();
    listeners.push(
        bridge::listen_voice_media_state(move |event| {
            let _ = event.native_aec;
            if event.state == "failed"
                && matches!(
                    media_state.voice_ui.get_untracked(),
                    VoiceUiState::Active { generation, .. } if generation == event.generation
                )
            {
                stop(media_state.clone(), protocol::VoiceStopReason::MediaFailed);
            }
        })
        .await?,
    );

    Ok(listeners)
}

pub fn install_media_listeners(state: AppState) {
    let token = begin_media_listener_lifecycle();
    on_cleanup(move || dispose_media_listener_lifecycle(token));

    spawn_local(async move {
        match register_media_listeners(state).await {
            Ok(listeners) => retain_media_listener_handles(token, listeners),
            Err(error) => {
                log::error!("failed to register native voice listeners: {error}");
            }
        }
    });
}

#[component]
fn VoiceControls(state: AppState) -> impl IntoView {
    move || {
        let start_state = state.clone();
        let stop_state = state.clone();
        let interrupt_state = state.clone();
        match state.voice_ui.get() {
            VoiceUiState::Idle => view! {
                <button
                    class="voice-icon"
                    on:click=move |_| start(start_state.clone())
                    aria-label="Start voice"
                >
                    "Voice"
                </button>
            }
            .into_any(),
            VoiceUiState::Starting { .. } => view! { <span>"Connecting voice…"</span> }.into_any(),
            VoiceUiState::Active {
                state: phase,
                transcript,
                ..
            } => view! {
                <span class="voice-pulse">{format!("{phase:?}")}</span>
                <span class="voice-transcript">
                    {transcript.map(|value| value.text).unwrap_or_default()}
                </span>
                <button on:click=move |_| interrupt(interrupt_state.clone())>
                    "Interrupt"
                </button>
                <button on:click=move |_| {
                    stop(
                        stop_state.clone(),
                        protocol::VoiceStopReason::UserExited,
                    )
                }>
                    "Done"
                </button>
            }
            .into_any(),
            VoiceUiState::Failed(error) => view! { <span>{error}</span> }.into_any(),
        }
    }
}

#[component]
pub fn VoiceOverlay() -> impl IntoView {
    let state = expect_context::<AppState>();
    install_media_listeners(state.clone());
    let previous = StoredValue::new(None::<ActiveAgentRef>);
    let switch_state = state.clone();
    Effect::new(move |_| {
        let active = switch_state.active_agent.get();
        if previous.get_value().is_some() && previous.get_value() != active {
            stop(
                switch_state.clone(),
                protocol::VoiceStopReason::TargetChanged,
            );
        }
        previous.set_value(active);
    });
    let gate_state_source = state.clone();
    let gate = Memo::new(move |_| {
        let (target_resolution, resolved_target) =
            target_with_resolution_tracked(&gate_state_source);
        let Some((host, _)) = resolved_target else {
            return voice_gate_state(target_resolution, false, false);
        };
        let voice_enabled = gate_state_source
            .host_settings_by_host
            .with(|settings| settings.get(&host).is_some_and(|value| value.voice.enabled));
        let capability = gate_state_source
            .voice_capabilities_by_host
            .with(|capabilities| capabilities.get(&host).cloned());
        voice_gate_state(
            target_resolution,
            voice_enabled,
            capability
                .as_ref()
                .is_some_and(|value| value.nova_available),
        )
    });
    let available = Memo::new(move |_| gate.get().gate_available);
    let render_state = StoredValue::new(state);
    view! {
        <Show when=move || available.get()>
            <aside class="voice-strip" data-testid="voice-strip">
                <VoiceControls state=render_state.get_value() />
            </aside>
        </Show>
    }
}

#[cfg(test)]
mod gate_tests {
    use super::*;

    fn active_downlink_state(generation: u64, next_output_media_seq: u64) -> VoiceUiState {
        VoiceUiState::Active {
            host_id: "local".to_string(),
            session_id: protocol::VoiceSessionId("voice-session".to_string()),
            target: protocol::VoiceTarget {
                agent_id: protocol::AgentId("agent".to_string()),
                instance_stream: StreamPath("/instance/local".to_string()),
            },
            generation,
            state: protocol::VoiceSessionState::Listening,
            transcript: None,
            next_output_media_seq,
            dropped_output_packets: 0,
        }
    }

    fn downlink_payload(generation: u64, first_media_seq: u64) -> protocol::VoiceAudioPayload {
        protocol::VoiceAudioPayload {
            session_id: protocol::VoiceSessionId("voice-session".to_string()),
            generation,
            direction: protocol::VoiceDirection::Output,
            first_media_seq,
            timestamp_samples_48k: first_media_seq.saturating_mul(960),
            packet_lengths: vec![3],
        }
    }

    #[test]
    fn target_resolution_reports_each_false_reason() {
        for (state, reason) in [
            (
                target_resolution_state(false, false, false, false),
                "active_agent_missing",
            ),
            (
                target_resolution_state(true, false, false, false),
                "matching_agent_missing",
            ),
            (
                target_resolution_state(true, true, false, false),
                "not_started",
            ),
            (
                target_resolution_state(true, true, true, true),
                "fatal_error",
            ),
        ] {
            assert!(!state.target_resolvable);
            assert_eq!(state.reason, reason);
        }
        assert_eq!(
            target_resolution_state(true, true, true, false).reason,
            "resolved"
        );
    }

    #[test]
    fn voice_gate_reports_each_false_conjunct_and_available() {
        let unresolved = target_resolution_state(false, false, false, false);
        let resolved = target_resolution_state(true, true, true, false);
        assert!(!voice_gate_state(unresolved, true, true).gate_available);
        assert!(!voice_gate_state(resolved, false, true).gate_available);
        assert!(!voice_gate_state(resolved, true, false).gate_available);
        assert_eq!(
            voice_gate_state(resolved, true, true),
            VoiceGateState {
                gate_available: true,
            }
        );
    }

    #[test]
    fn downlink_admission_classifies_every_webview_outcome() {
        let payload = downlink_payload(7, 5);
        let mut inactive = VoiceUiState::Idle;
        assert_eq!(
            admit_voice_downlink(&mut inactive, &payload),
            Err((7, VoiceDownlinkRejection::Inactive,))
        );

        let mut other_generation = active_downlink_state(8, 0);
        assert_eq!(
            admit_voice_downlink(&mut other_generation, &payload),
            Err((8, VoiceDownlinkRejection::Generation,))
        );

        let mut stale_sequence = active_downlink_state(7, 6);
        assert_eq!(
            admit_voice_downlink(&mut stale_sequence, &payload),
            Err((7, VoiceDownlinkRejection::Sequence,))
        );

        let mut accepted = active_downlink_state(7, 3);
        assert_eq!(admit_voice_downlink(&mut accepted, &payload), Ok(7));
        let VoiceUiState::Active {
            next_output_media_seq,
            dropped_output_packets,
            ..
        } = accepted
        else {
            panic!("accepted downlink must preserve the active state");
        };
        assert_eq!(next_output_media_seq, 6);
        assert_eq!(dropped_output_packets, 2);
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::state::TabContent;
    use leptos::mount::mount_to;
    use protocol::{AgentId, AgentOrigin, BackendKind};
    use wasm_bindgen::JsCast;
    use wasm_bindgen_futures::JsFuture;
    use wasm_bindgen_test::*;
    use web_sys::HtmlElement;

    wasm_bindgen_test_configure!(run_in_browser);

    fn container() -> HtmlElement {
        let document = web_sys::window().unwrap().document().unwrap();
        let element = document.create_element("div").unwrap();
        document.body().unwrap().append_child(&element).unwrap();
        element.dyn_into().unwrap()
    }

    async fn next_tick() {
        let promise = js_sys::Promise::new(&mut |resolve, _| {
            web_sys::window()
                .unwrap()
                .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 0)
                .unwrap();
        });
        let _ = JsFuture::from(promise).await;
    }

    struct WindowPropertySnapshot {
        name: &'static str,
        existed: bool,
        value: wasm_bindgen::JsValue,
    }

    impl WindowPropertySnapshot {
        fn capture(name: &'static str) -> Self {
            let window = web_sys::window().expect("window");
            let key = wasm_bindgen::JsValue::from_str(name);
            Self {
                name,
                existed: js_sys::Reflect::has(&window, &key).unwrap_or(false),
                value: js_sys::Reflect::get(&window, &key)
                    .unwrap_or(wasm_bindgen::JsValue::UNDEFINED),
            }
        }

        fn restore(&self) {
            let window = web_sys::window().expect("window");
            let key = wasm_bindgen::JsValue::from_str(self.name);
            if self.existed {
                let _ = js_sys::Reflect::set(&window, &key, &self.value);
            } else {
                let target: &js_sys::Object = window.unchecked_ref();
                let _ = js_sys::Reflect::delete_property(target, &key);
            }
        }
    }

    struct VoiceListenerStubGuard {
        properties: Vec<WindowPropertySnapshot>,
    }

    impl Drop for VoiceListenerStubGuard {
        fn drop(&mut self) {
            for property in self.properties.iter().rev() {
                property.restore();
            }
        }
    }

    fn install_voice_listener_stub(
        fail_at: Option<u32>,
        defer_at: Option<u32>,
    ) -> VoiceListenerStubGuard {
        let properties = [
            "__TAURI__",
            "__tyde_voice_test_packet_dispatch",
            "__tyde_voice_test_host_frame_dispatch",
            "__tyde_voice_test_send_host_frame_args",
            "__tyde_voice_test_push_output_args",
            "__tyde_voice_test_listen_attempts",
            "__tyde_voice_test_unlisten_calls",
            "__tyde_voice_test_active_listeners",
            "__tyde_voice_test_fail_listener_at",
            "__tyde_voice_test_defer_listener_at",
            "__tyde_voice_test_release_listener",
        ]
        .into_iter()
        .map(WindowPropertySnapshot::capture)
        .collect();
        js_sys::eval(
            r#"
            window.__tyde_voice_test_packet_dispatch = undefined;
            window.__tyde_voice_test_host_frame_dispatch = undefined;
            window.__tyde_voice_test_send_host_frame_args = undefined;
            window.__tyde_voice_test_push_output_args = undefined;
            window.__tyde_voice_test_listen_attempts = 0;
            window.__tyde_voice_test_unlisten_calls = 0;
            window.__tyde_voice_test_active_listeners = 0;
            window.__tyde_voice_test_release_listener = undefined;
            window.__TAURI__ = {
                event: {
                    listen: function(name, handler) {
                        window.__tyde_voice_test_listen_attempts += 1;
                        if (window.__tyde_voice_test_fail_listener_at ===
                            window.__tyde_voice_test_listen_attempts) {
                            return Promise.reject(new Error("synthetic registration failure"));
                        }
                        let active = true;
                        window.__tyde_voice_test_active_listeners += 1;
                        if (name === "tyde://voice-opus-packet") {
                            window.__tyde_voice_test_packet_dispatch = function(event) {
                                if (active) {
                                    handler(event);
                                }
                            };
                        }
                        if (name === "tyde://host-voice-frame") {
                            window.__tyde_voice_test_host_frame_dispatch = function(event) {
                                if (active) {
                                    handler(event);
                                }
                            };
                        }
                        const unlisten = function() {
                            if (active) {
                                active = false;
                                window.__tyde_voice_test_active_listeners -= 1;
                                window.__tyde_voice_test_unlisten_calls += 1;
                            }
                        };
                        if (window.__tyde_voice_test_defer_listener_at ===
                            window.__tyde_voice_test_listen_attempts) {
                            return new Promise(function(resolve) {
                                window.__tyde_voice_test_release_listener = function() {
                                    window.__tyde_voice_test_release_listener = undefined;
                                    resolve(unlisten);
                                };
                            });
                        }
                        return Promise.resolve(unlisten);
                    }
                },
                core: {
                    invoke: function(command, args) {
                        if (command === "send_host_frame") {
                            window.__tyde_voice_test_send_host_frame_args = args;
                            return Promise.resolve(null);
                        }
                        if (command === "voice_media_push_output") {
                            const keys = Object.keys(args).sort();
                            const expected = ["generation", "mediaSeq", "opus", "timestampSamples48k"];
                            const bytes = Array.isArray(args.opus) ? args.opus : [];
                            if (JSON.stringify(keys) !== JSON.stringify(expected) ||
                                !Number.isSafeInteger(args.generation) ||
                                !Number.isSafeInteger(args.mediaSeq) ||
                                !Number.isSafeInteger(args.timestampSamples48k) ||
                                bytes.length === 0 ||
                                bytes.some(value => !Number.isInteger(value) || value < 0 || value > 255)) {
                                return Promise.reject(new Error("invalid voice output invoke contract"));
                            }
                            window.__tyde_voice_test_push_output_args = {
                                keys,
                                generation: args.generation,
                                mediaSeq: args.mediaSeq,
                                timestampSamples48k: args.timestampSamples48k,
                                opus: bytes.slice()
                            };
                            return Promise.resolve(null);
                        }
                        return Promise.resolve(null);
                    }
                }
            };
            "#,
        )
        .expect("install voice listener stub");
        js_sys::Reflect::set(
            &web_sys::window().expect("window"),
            &wasm_bindgen::JsValue::from_str("__tyde_voice_test_fail_listener_at"),
            &fail_at
                .map(|value| wasm_bindgen::JsValue::from_f64(value.into()))
                .unwrap_or(wasm_bindgen::JsValue::NULL),
        )
        .expect("set listener failure point");
        js_sys::Reflect::set(
            &web_sys::window().expect("window"),
            &wasm_bindgen::JsValue::from_str("__tyde_voice_test_defer_listener_at"),
            &defer_at
                .map(|value| wasm_bindgen::JsValue::from_f64(value.into()))
                .unwrap_or(wasm_bindgen::JsValue::NULL),
        )
        .expect("set deferred listener point");
        VoiceListenerStubGuard { properties }
    }

    fn release_deferred_voice_listener() {
        let release = js_sys::Reflect::get(
            &web_sys::window().expect("window"),
            &wasm_bindgen::JsValue::from_str("__tyde_voice_test_release_listener"),
        )
        .expect("read deferred listener resolver")
        .dyn_into::<js_sys::Function>()
        .expect("deferred listener resolver installed");
        release
            .call0(&wasm_bindgen::JsValue::NULL)
            .expect("release deferred listener");
    }

    fn voice_listener_counter(name: &str) -> u32 {
        js_sys::Reflect::get(
            &web_sys::window().expect("window"),
            &wasm_bindgen::JsValue::from_str(name),
        )
        .expect("read voice listener counter")
        .as_f64()
        .expect("numeric voice listener counter") as u32
    }

    async fn wait_for_voice_listener_attempts(expected: u32) {
        for _ in 0..8 {
            if voice_listener_counter("__tyde_voice_test_listen_attempts") == expected {
                return;
            }
            next_tick().await;
        }
        assert_eq!(
            voice_listener_counter("__tyde_voice_test_listen_attempts"),
            expected
        );
    }

    fn dispatch_tauri_voice_event<T: serde::Serialize>(property: &str, event: &str, payload: T) {
        #[derive(serde::Serialize)]
        struct TauriEventFixture<'a, T> {
            event: &'a str,
            id: u32,
            payload: T,
        }

        let handler = js_sys::Reflect::get(
            &web_sys::window().expect("window"),
            &wasm_bindgen::JsValue::from_str(property),
        )
        .expect("read installed voice listener")
        .dyn_into::<js_sys::Function>()
        .expect("voice listener must be callable");
        let event = serde_wasm_bindgen::to_value(&TauriEventFixture {
            event,
            id: 17,
            payload,
        })
        .expect("serialize production Tauri event envelope");
        handler
            .call1(&wasm_bindgen::JsValue::NULL, &event)
            .expect("dispatch production Tauri event envelope");
    }

    fn captured_invoke(name: &str) -> wasm_bindgen::JsValue {
        js_sys::Reflect::get(
            &web_sys::window().expect("window"),
            &wasm_bindgen::JsValue::from_str(name),
        )
        .expect("read captured Tauri invocation")
    }

    fn gate_test_agent(started: bool) -> AgentInfo {
        AgentInfo {
            host_id: "local".to_owned(),
            agent_id: AgentId("voice-agent".to_owned()),
            name: "Voice agent".to_owned(),
            origin: AgentOrigin::User,
            backend_kind: BackendKind::Claude,
            workspace_roots: Vec::new(),
            project_id: None,
            parent_agent_id: None,
            team_member_id: None,
            session_id: None,
            custom_agent_id: None,
            workflow: None,
            created_at_ms: 0,
            instance_stream: StreamPath("/agent/voice-agent/instance".to_owned()),
            started,
            fatal_error: None,
            activity_summary: Default::default(),
        }
    }

    fn active_voice_state(generation: u64) -> VoiceUiState {
        VoiceUiState::Active {
            generation,
            host_id: "local".to_owned(),
            session_id: protocol::VoiceSessionId("voice-session".to_owned()),
            target: protocol::VoiceTarget {
                agent_id: AgentId("voice-agent".to_owned()),
                instance_stream: StreamPath("/agent/voice-agent/instance".to_owned()),
            },
            state: protocol::VoiceSessionState::Listening,
            transcript: None,
            next_output_media_seq: 0,
            dropped_output_packets: 0,
        }
    }

    fn voice_packet(generation: u64) -> bridge::VoiceOpusPacketEvent {
        bridge::VoiceOpusPacketEvent {
            generation,
            media_seq: 4,
            timestamp_samples_48k: 3_840,
            opus: vec![1, 2, 3],
        }
    }

    #[wasm_bindgen_test]
    fn native_packet_inactive_drop_is_classified_before_send() {
        assert!(matches!(
            prepare_voice_uplink(&VoiceUiState::Idle, voice_packet(7)),
            Err(VoiceUplinkRejection::Inactive)
        ));
    }

    #[wasm_bindgen_test]
    fn native_packet_generation_drop_is_classified_before_send() {
        assert!(matches!(
            prepare_voice_uplink(&active_voice_state(8), voice_packet(7)),
            Err(VoiceUplinkRejection::Generation)
        ));
    }

    #[wasm_bindgen_test]
    async fn active_native_packet_classifies_success_after_tyd2_send() {
        let prepared = prepare_voice_uplink(&active_voice_state(7), voice_packet(7)).unwrap();
        let outcome = send_prepared_voice_uplink_with(
            prepared,
            |host_id, stream, payload, opus| async move {
                assert_eq!(host_id, "local");
                assert_eq!(stream.0, "/voice/voice-session");
                assert_eq!(payload.direction, protocol::VoiceDirection::Input);
                assert_eq!(payload.first_media_seq, 4);
                assert_eq!(opus, vec![1, 2, 3]);
                Ok(())
            },
        )
        .await;
        assert_eq!(outcome, Ok(()));
    }

    #[wasm_bindgen_test]
    async fn active_native_packet_classifies_send_failure_without_error_text() {
        let prepared = prepare_voice_uplink(&active_voice_state(7), voice_packet(7)).unwrap();
        let outcome = send_prepared_voice_uplink_with(prepared, |_, _, _, _| async move {
            Err("sensitive transport detail".to_owned())
        })
        .await;
        assert_eq!(outcome, Err("sensitive transport detail".to_owned()));
    }

    #[wasm_bindgen_test]
    async fn output_bridge_serializes_the_registered_command_contract() {
        let _stub = install_voice_listener_stub(None, None);
        bridge::voice_media_push_output(7, 9, 8_640, &[1, 2, 255])
            .await
            .expect("camel-case output command arguments must dispatch");

        let captured = js_sys::Reflect::get(
            &web_sys::window().expect("window"),
            &wasm_bindgen::JsValue::from_str("__tyde_voice_test_push_output_args"),
        )
        .expect("read captured output command arguments");
        let keys = js_sys::Reflect::get(&captured, &wasm_bindgen::JsValue::from_str("keys"))
            .expect("read output command keys");
        assert_eq!(
            serde_wasm_bindgen::from_value::<Vec<String>>(keys).expect("decode command keys"),
            vec!["generation", "mediaSeq", "opus", "timestampSamples48k"]
        );
        for (name, expected) in [
            ("generation", 7.0),
            ("mediaSeq", 9.0),
            ("timestampSamples48k", 8_640.0),
        ] {
            assert_eq!(
                js_sys::Reflect::get(&captured, &wasm_bindgen::JsValue::from_str(name))
                    .expect("read numeric output argument")
                    .as_f64(),
                Some(expected)
            );
        }
        let opus = js_sys::Reflect::get(&captured, &wasm_bindgen::JsValue::from_str("opus"))
            .expect("read serialized Opus bytes");
        assert_eq!(
            serde_wasm_bindgen::from_value::<Vec<u8>>(opus).expect("decode Opus bytes"),
            vec![1, 2, 255]
        );
    }

    #[wasm_bindgen_test]
    async fn mounted_production_listeners_route_uplink_and_downlink_invokes() {
        #[derive(serde::Serialize)]
        #[serde(rename_all = "camelCase")]
        struct UplinkEvent {
            generation: u64,
            media_seq: u64,
            timestamp_samples_48k: u64,
            opus: Vec<u8>,
        }

        #[derive(serde::Serialize)]
        #[serde(rename_all = "camelCase")]
        struct DownlinkEvent {
            host_id: String,
            envelope: String,
            opus: Vec<u8>,
        }

        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct SendHostFrameInvocation {
            host_id: String,
            envelope: String,
            binary: Vec<u8>,
        }

        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct PushOutputInvocation {
            generation: u64,
            media_seq: u64,
            timestamp_samples_48k: u64,
            opus: Vec<u8>,
        }

        let _stub = install_voice_listener_stub(None, None);
        let container = container();
        let state = AppState::new();
        state.voice_ui.set(active_voice_state(7));
        let render_state = state.clone();
        let mount = mount_to(container.clone(), move || {
            provide_context(render_state.clone());
            view! { <VoiceOverlay /> }
        });
        wait_for_voice_listener_attempts(4).await;
        next_tick().await;

        dispatch_tauri_voice_event(
            "__tyde_voice_test_packet_dispatch",
            "tyde://voice-opus-packet",
            UplinkEvent {
                generation: 7,
                media_seq: 4,
                timestamp_samples_48k: 3_840,
                opus: vec![1, 2, 3],
            },
        );
        next_tick().await;
        let uplink: SendHostFrameInvocation = serde_wasm_bindgen::from_value(captured_invoke(
            "__tyde_voice_test_send_host_frame_args",
        ))
        .expect("decode send_host_frame invocation");
        assert_eq!(uplink.host_id, "local");
        assert_eq!(uplink.binary, vec![1, 2, 3]);
        let envelope: protocol::Envelope =
            serde_json::from_str(&uplink.envelope).expect("decode uplink envelope");
        assert_eq!(envelope.kind, FrameKind::VoiceAudio);
        let payload: protocol::VoiceAudioPayload =
            envelope.parse_payload().expect("decode uplink payload");
        assert_eq!(payload.direction, protocol::VoiceDirection::Input);
        assert_eq!(payload.generation, 7);
        assert_eq!(payload.first_media_seq, 4);

        let downlink_payload = protocol::VoiceAudioPayload {
            session_id: protocol::VoiceSessionId("voice-session".to_owned()),
            generation: 7,
            direction: protocol::VoiceDirection::Output,
            first_media_seq: 9,
            timestamp_samples_48k: 8_640,
            packet_lengths: vec![2],
        };
        let downlink_envelope = protocol::Envelope::from_payload(
            StreamPath("/voice/voice-session".to_owned()),
            FrameKind::VoiceAudio,
            1,
            &downlink_payload,
        )
        .expect("build downlink envelope");
        dispatch_tauri_voice_event(
            "__tyde_voice_test_host_frame_dispatch",
            "tyde://host-voice-frame",
            DownlinkEvent {
                host_id: "local".to_owned(),
                envelope: serde_json::to_string(&downlink_envelope)
                    .expect("encode downlink envelope"),
                opus: vec![4, 5],
            },
        );
        next_tick().await;
        let downlink: PushOutputInvocation =
            serde_wasm_bindgen::from_value(captured_invoke("__tyde_voice_test_push_output_args"))
                .expect("decode voice_media_push_output invocation");
        assert_eq!(downlink.generation, 7);
        assert_eq!(downlink.media_seq, 9);
        assert_eq!(downlink.timestamp_samples_48k, 8_640);
        assert_eq!(downlink.opus, vec![4, 5]);

        drop(mount);
        container.remove();
    }

    #[wasm_bindgen_test]
    async fn partial_voice_listener_registration_unlistens_prior_handles() {
        let _stub = install_voice_listener_stub(Some(3), None);
        let container = container();
        let state = AppState::new();
        let mount = mount_to(container.clone(), move || {
            provide_context(state.clone());
            view! { <VoiceOverlay /> }
        });

        wait_for_voice_listener_attempts(3).await;
        next_tick().await;
        assert_eq!(
            voice_listener_counter("__tyde_voice_test_unlisten_calls"),
            2,
            "a failed third registration must clean the first two handles"
        );
        assert_eq!(
            voice_listener_counter("__tyde_voice_test_active_listeners"),
            0
        );
        drop(mount);
        assert_eq!(
            voice_listener_counter("__tyde_voice_test_unlisten_calls"),
            2,
            "component cleanup must not unlisten partial handles twice"
        );
        container.remove();
    }

    #[wasm_bindgen_test]
    async fn registration_success_after_unmount_unlistens_every_late_handle() {
        let _stub = install_voice_listener_stub(None, Some(4));
        let container = container();
        let state = AppState::new();
        let mount = mount_to(container.clone(), move || {
            provide_context(state.clone());
            view! { <VoiceOverlay /> }
        });

        wait_for_voice_listener_attempts(4).await;
        assert_eq!(
            voice_listener_counter("__tyde_voice_test_active_listeners"),
            4
        );
        assert_eq!(
            voice_listener_counter("__tyde_voice_test_unlisten_calls"),
            0
        );

        drop(mount);
        release_deferred_voice_listener();
        next_tick().await;
        next_tick().await;
        assert_eq!(
            voice_listener_counter("__tyde_voice_test_unlisten_calls"),
            4,
            "late successful registration must drop the complete stale owner"
        );
        assert_eq!(
            voice_listener_counter("__tyde_voice_test_active_listeners"),
            0
        );

        assert_eq!(
            voice_listener_counter("__tyde_voice_test_active_listeners"),
            0,
            "a stale completed registration must not revive after teardown"
        );
        container.remove();
    }

    #[wasm_bindgen_test]
    async fn controls_react_across_repeated_renders() {
        let container = container();
        let state = AppState::new();
        let render_state = state.clone();
        let mount = mount_to(container.clone(), move || {
            view! { <VoiceControls state=render_state.clone() /> }
        });
        next_tick().await;
        assert!(container.query_selector(".voice-icon").unwrap().is_some());

        state
            .voice_ui
            .set(VoiceUiState::Failed("voice unavailable".into()));
        next_tick().await;
        assert!(
            container
                .text_content()
                .unwrap()
                .contains("voice unavailable")
        );

        state.voice_ui.set(VoiceUiState::Idle);
        next_tick().await;
        assert!(container.query_selector(".voice-icon").unwrap().is_some());
        drop(mount);
        container.remove();
    }

    #[wasm_bindgen_test]
    async fn overlay_reacts_from_unresolved_target_to_available_and_fatal() {
        let container = container();
        let state = AppState::new();
        state.host_settings_by_host.update(|settings| {
            let mut host = protocol::HostSettings::default();
            host.voice.enabled = true;
            settings.insert("local".to_owned(), host);
        });
        state.voice_capabilities_by_host.update(|capabilities| {
            capabilities.insert(
                "local".to_owned(),
                protocol::VoiceCapabilitiesPayload::for_connection(true, true),
            );
        });
        let render_state = state.clone();
        let mount = mount_to(container.clone(), move || {
            provide_context(render_state.clone());
            view! { <VoiceOverlay /> }
        });
        next_tick().await;

        assert!(
            container
                .query_selector("[data-testid='voice-strip']")
                .unwrap()
                .is_none()
        );

        state.open_tab(
            TabContent::chat_with_agent(ActiveAgentRef {
                host_id: "local".to_owned(),
                agent_id: AgentId("voice-agent".to_owned()),
            }),
            "Voice agent".to_owned(),
            true,
        );
        next_tick().await;

        state.agents.set(vec![gate_test_agent(false)]);
        next_tick().await;

        state.agents.update(|agents| agents[0].started = true);
        next_tick().await;
        assert!(
            container
                .query_selector("[data-testid='voice-strip'] .voice-icon")
                .unwrap()
                .is_some()
        );

        state.agents.update(|agents| {
            agents[0].fatal_error = Some("not exposed".to_owned());
        });
        next_tick().await;
        assert!(
            container
                .query_selector("[data-testid='voice-strip']")
                .unwrap()
                .is_none()
        );

        drop(mount);
        container.remove();
    }
}
