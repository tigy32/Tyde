use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, VecDeque},
};

use leptos::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;

use crate::{
    bridge,
    send::{send_binary_frame, send_frame},
    state::{ActiveAgentRef, AgentInfo, AppState, ComposerHandle},
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

enum VoiceUplinkRouting {
    Send(PreparedVoiceUplink),
    /// Captured before the host accepted the session. The packet waits in the
    /// queue rather than being dropped, which is the whole point of opening the
    /// microphone on the press.
    Hold(bridge::VoiceOpusPacketEvent),
    Drop,
}

fn route_voice_uplink(
    state: &VoiceUiState,
    event: bridge::VoiceOpusPacketEvent,
) -> VoiceUplinkRouting {
    if let VoiceUiState::Starting { generation, .. } = state
        && *generation == event.generation
    {
        return VoiceUplinkRouting::Hold(event);
    }
    match prepare_voice_uplink(state, event) {
        Ok(prepared) => VoiceUplinkRouting::Send(prepared),
        Err(_) => VoiceUplinkRouting::Drop,
    }
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

/// The three voices of a session, each pinned to its own display lane so the
/// band can show what the model is hearing (you), what it is saying (Nova),
/// and what the coding agent is doing — instead of one interleaved stream.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct VoiceLanes {
    pub you: Option<String>,
    pub nova: Option<String>,
    pub agent: Option<String>,
}

#[derive(Clone)]
pub struct DictationCapture {
    pub composer_text: ArcRwSignal<String>,
    pub finalized: String,
    pub partial: Option<String>,
    pub finishing: bool,
}

#[derive(Clone)]
pub enum VoiceUiState {
    Idle,
    Starting {
        generation: u64,
        host_id: String,
        request: protocol::VoiceRequest,
        dictation: Option<DictationCapture>,
    },
    Active {
        generation: u64,
        host_id: String,
        session_id: protocol::VoiceSessionId,
        request: protocol::VoiceAcceptedRequest,
        dictation: Option<DictationCapture>,
        state: protocol::VoiceSessionState,
        lanes: VoiceLanes,
        next_output_media_seq: u64,
        dropped_output_packets: u64,
    },
    Failed {
        error: String,
        composer_text: Option<ArcRwSignal<String>>,
    },
}

fn failed_voice_state(current: &VoiceUiState, error: String) -> VoiceUiState {
    let composer_text = match current {
        VoiceUiState::Starting {
            dictation: Some(dictation),
            ..
        }
        | VoiceUiState::Active {
            dictation: Some(dictation),
            ..
        } => Some(dictation.composer_text.clone()),
        _ => None,
    };
    VoiceUiState::Failed {
        error,
        composer_text,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TargetResolutionState {
    target_resolvable: bool,
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
    }
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

pub fn start_conversation(state: AppState) {
    let Some((host_id, target)) = target(&state) else {
        return;
    };
    start(
        state,
        host_id,
        protocol::VoiceRequest::Conversation {
            target,
            formats: vec![protocol::VoiceFormatPair {
                uplink: protocol::VoiceAudioFormat::opus(48_000),
                downlink: protocol::VoiceAudioFormat::opus(24_000),
            }],
        },
        None,
    );
}

pub fn start_dictation(state: AppState, host_id: String, composer_text: ArcRwSignal<String>) {
    start(
        state,
        host_id,
        protocol::VoiceRequest::Dictation {
            formats: vec![protocol::VoiceAudioFormat::opus(48_000)],
        },
        Some(DictationCapture {
            composer_text,
            finalized: String::new(),
            partial: None,
            finishing: false,
        }),
    );
}

fn start(
    state: AppState,
    host_id: String,
    request: protocol::VoiceRequest,
    dictation: Option<DictationCapture>,
) {
    let generation = state
        .voice_generation
        .try_update(|value| {
            *value = value.saturating_add(1);
            *value
        })
        .unwrap_or(1);
    clear_voice_uplink_queue();
    state.voice_finish_pending.set(false);
    let capture_first = request.mode() == protocol::VoiceMode::Dictation;
    state.voice_ui.set(VoiceUiState::Starting {
        generation,
        host_id: host_id.clone(),
        request: request.clone(),
        dictation,
    });
    if capture_first {
        // Open the microphone on the user's gesture instead of waiting for the
        // host to accept. Credential resolution and the Transcribe stream open
        // sit between the press and acceptance, and anything said in that
        // window was previously never captured at all. What is recorded before
        // acceptance waits in the uplink queue, and is discarded with it if the
        // session never starts.
        let capture_state = state.clone();
        let capture_host = host_id.clone();
        spawn_local(async move {
            if let Err(error) =
                bridge::voice_media_start(&capture_host, generation, true, true).await
            {
                let failed = failed_voice_state(&capture_state.voice_ui.get_untracked(), error);
                stop(
                    capture_state.clone(),
                    protocol::VoiceStopReason::MediaFailed,
                );
                capture_state.voice_ui.set(failed);
            }
        });
    }
    spawn_local(async move {
        let payload = protocol::VoiceStartPayload {
            generation,
            request,
        };
        if let Err(error) = send_frame(
            &host_id,
            StreamPath("/voice".into()),
            FrameKind::VoiceStart,
            &payload,
        )
        .await
        {
            let failed = failed_voice_state(&state.voice_ui.get_untracked(), error);
            state.voice_ui.set(failed);
        }
    });
}

pub fn stop(state: AppState, reason: protocol::VoiceStopReason) {
    let current = state.voice_ui.get_untracked();
    clear_voice_uplink_queue();
    state.voice_finish_pending.set(false);
    let VoiceUiState::Active {
        generation,
        host_id,
        session_id,
        dropped_output_packets,
        ..
    } = current
    else {
        state.voice_ui.set(VoiceUiState::Idle);
        // A session abandoned before acceptance still has a live microphone,
        // because capture-first opens it on the press. Leaving without this
        // would keep it recording with nowhere to send the audio.
        spawn_local(async {
            let _ = bridge::voice_media_stop().await;
        });
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

/// Release of a hold-to-talk press. Capture stops immediately, because the user
/// let go, but the turn only ends once everything already captured has been
/// sent — including audio recorded before the host accepted the session.
pub fn request_dictation_finish(state: AppState) {
    if matches!(state.voice_ui.get_untracked(), VoiceUiState::Idle) {
        return;
    }
    state.voice_finish_pending.set(true);
    let media_state = state.clone();
    spawn_local(async move {
        let _ = bridge::voice_media_stop().await;
        drain_voice_uplink_queue(&media_state);
    });
    drain_voice_uplink_queue(&state);
}

pub fn finish_dictation(state: AppState) {
    let VoiceUiState::Active {
        generation,
        ref host_id,
        ref session_id,
        request: protocol::VoiceAcceptedRequest::Dictation { .. },
        ..
    } = state.voice_ui.get_untracked()
    else {
        return;
    };
    let host_id = host_id.clone();
    let session_id = session_id.clone();
    state.voice_ui.update(|current| {
        if let VoiceUiState::Active {
            state, dictation, ..
        } = current
        {
            *state = protocol::VoiceSessionState::Ending;
            if let Some(dictation) = dictation {
                dictation.finishing = true;
                dictation.partial = None;
            }
        }
    });
    spawn_local(async move {
        let _ = bridge::voice_media_stop().await;
        let payload = protocol::VoiceSessionPayload {
            session_id: session_id.clone(),
            generation,
        };
        if let Err(error) = send_frame(
            &host_id,
            StreamPath(format!("/voice/{}", session_id.0)),
            FrameKind::VoiceInputEnd,
            &payload,
        )
        .await
        {
            let failed = failed_voice_state(&state.voice_ui.get_untracked(), error);
            state.voice_ui.set(failed);
        }
    });
}

pub fn interrupt(state: AppState) {
    let VoiceUiState::Active {
        generation,
        host_id,
        session_id,
        request: protocol::VoiceAcceptedRequest::Conversation { .. },
        ..
    } = state.voice_ui.get_untracked()
    else {
        return;
    };
    enqueue_media_item(&state, QueuedMediaItem::Flush { generation });
    spawn_local(async move {
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

fn append_provider_text(destination: &mut String, text: &str) {
    if text.is_empty() {
        return;
    }
    if !destination.is_empty()
        && !destination.chars().last().is_some_and(char::is_whitespace)
        && !text.chars().next().is_some_and(char::is_whitespace)
    {
        destination.push(' ');
    }
    destination.push_str(text);
}

pub fn handle_control(state: &AppState, host_id: &str, envelope: &protocol::Envelope) {
    match envelope.kind {
        FrameKind::VoiceAccepted => {
            if let Ok(payload) = envelope.parse_payload::<protocol::VoiceAcceptedPayload>() {
                let pending = state.voice_ui.get_untracked();
                let (valid, dictation) = match pending {
                    VoiceUiState::Starting {
                        generation,
                        host_id: pending_host,
                        request:
                            protocol::VoiceRequest::Conversation {
                                target: pending_target,
                                ..
                            },
                        dictation,
                    } => {
                        let accepted_target = match &payload.request {
                            protocol::VoiceAcceptedRequest::Conversation { target, .. } => {
                                Some(target)
                            }
                            protocol::VoiceAcceptedRequest::Dictation { .. } => None,
                        };
                        (
                            generation == payload.generation
                                && pending_host == host_id
                                && accepted_target == Some(&pending_target)
                                && target(state).is_some_and(|(active_host, active_target)| {
                                    active_host == host_id && active_target == pending_target
                                }),
                            dictation,
                        )
                    }
                    VoiceUiState::Starting {
                        generation,
                        host_id: pending_host,
                        request: protocol::VoiceRequest::Dictation { .. },
                        dictation,
                    } => (
                        generation == payload.generation
                            && pending_host == host_id
                            && payload.request.mode() == protocol::VoiceMode::Dictation
                            && dictation.is_some(),
                        dictation,
                    ),
                    _ => (false, None),
                };
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
                let input_only = payload.request.mode() == protocol::VoiceMode::Dictation;
                state.voice_ui.set(VoiceUiState::Active {
                    generation,
                    host_id: host_id.into(),
                    session_id: payload.session_id,
                    request: payload.request,
                    dictation,
                    state: protocol::VoiceSessionState::Listening,
                    lanes: VoiceLanes::default(),
                    next_output_media_seq: 0,
                    dropped_output_packets: 0,
                });
                if input_only {
                    // Capture-first already opened the microphone on the press.
                    // Everything recorded while connecting is queued, and now
                    // flushes in capture order ahead of anything since.
                    drain_voice_uplink_queue(state);
                } else {
                    let media_state = state.clone();
                    let media_host = host_id.to_owned();
                    spawn_local(async move {
                        if let Err(error) =
                            bridge::voice_media_start(&media_host, generation, input_only, false)
                                .await
                        {
                            let failed =
                                failed_voice_state(&media_state.voice_ui.get_untracked(), error);
                            stop(media_state.clone(), protocol::VoiceStopReason::MediaFailed);
                            media_state.voice_ui.set(failed);
                        }
                    });
                }
            }
        }
        FrameKind::VoiceTranscript => {
            if let Ok(transcript) = envelope.parse_payload::<protocol::VoiceTranscriptPayload>() {
                let active_mode = match state.voice_ui.get_untracked() {
                    VoiceUiState::Active {
                        generation,
                        ref session_id,
                        ref request,
                        ..
                    } if generation == transcript.generation
                        && session_id == &transcript.session_id =>
                    {
                        Some(request.clone())
                    }
                    _ => None,
                };
                let Some(active_request) = active_mode else {
                    return;
                };
                if active_request.mode() == protocol::VoiceMode::Dictation {
                    if transcript.speaker != protocol::VoiceTranscriptSpeaker::User
                        || transcript.message_id.is_some()
                    {
                        return;
                    }
                    state.voice_ui.update(|current| {
                        if let VoiceUiState::Active {
                            dictation: Some(dictation),
                            ..
                        } = current
                        {
                            if transcript.is_final {
                                append_provider_text(&mut dictation.finalized, &transcript.text);
                                dictation.partial = None;
                            } else {
                                dictation.partial = Some(transcript.text.clone());
                            }
                        }
                    });
                    return;
                }
                let protocol::VoiceAcceptedRequest::Conversation { target, .. } = active_request
                else {
                    return;
                };
                state.voice_ui.update(|current| {
                    if let VoiceUiState::Active { lanes, .. } = current {
                        let slot = match transcript.speaker {
                            protocol::VoiceTranscriptSpeaker::User => &mut lanes.you,
                            protocol::VoiceTranscriptSpeaker::Assistant => &mut lanes.nova,
                            protocol::VoiceTranscriptSpeaker::Progress => &mut lanes.agent,
                        };
                        *slot = Some(transcript.text.clone());
                    }
                });
                if transcript.is_final && state.voice_transcript_in_chat.get_untracked() {
                    state.push_chat_entry(
                        target.agent_id,
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
                    enqueue_media_item(state, QueuedMediaItem::Flush { generation });
                }
            }
        }
        FrameKind::VoiceStop => {
            if let Ok(payload) = envelope.parse_payload::<protocol::VoiceStopPayload>()
                && matches!(state.voice_ui.get_untracked(),VoiceUiState::Active{generation,ref session_id,..} if generation==payload.generation&&session_id==&payload.session_id)
            {
                let completed_dictation = match state.voice_ui.get_untracked() {
                    VoiceUiState::Active {
                        request: protocol::VoiceAcceptedRequest::Dictation { .. },
                        dictation: Some(dictation),
                        ..
                    } if payload.reason == protocol::VoiceStopReason::ProviderCompleted
                        && dictation.finishing =>
                    {
                        Some(dictation)
                    }
                    _ => None,
                };
                if let Some(dictation) = completed_dictation {
                    dictation
                        .composer_text
                        .update(|draft| append_provider_text(draft, &dictation.finalized));
                }
                state.voice_ui.set(VoiceUiState::Idle);
                clear_media_push_queue();
                clear_voice_uplink_queue();
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
                    let failed = failed_voice_state(
                        &state.voice_ui.get_untracked(),
                        voice_error_message(&payload),
                    );
                    state.voice_ui.set(failed);
                    clear_media_push_queue();
                    clear_voice_uplink_queue();
                    spawn_local(async {
                        let _ = bridge::voice_media_stop().await;
                    });
                }
            }
        }
        _ => {}
    }
}

/// User-facing description of a session-ending voice error. The wire payload
/// carries only a category code — the provider detail is in the host log — so
/// this names the failure and whether retrying is worthwhile.
fn voice_error_message(payload: &protocol::VoiceErrorPayload) -> String {
    let cause = match payload.code {
        protocol::VoiceErrorCode::CredentialsExpired => {
            "AWS credentials expired — refresh them and try again"
        }
        protocol::VoiceErrorCode::MissingCredentials => {
            "AWS credentials are not available on the host"
        }
        protocol::VoiceErrorCode::PermissionDenied => {
            "the AWS profile lacks permission for this speech service"
        }
        protocol::VoiceErrorCode::QuotaExceeded => {
            "the AWS speech service quota or concurrency limit was reached"
        }
        protocol::VoiceErrorCode::InvalidConfiguration => {
            "the configured AWS region or dictation language was rejected"
        }
        protocol::VoiceErrorCode::NotAvailable => "voice is not available for this host",
        protocol::VoiceErrorCode::ProviderUnavailable => {
            "the configured Amazon speech provider dropped the session"
        }
        protocol::VoiceErrorCode::Inactivity => "the session ended after inactivity",
        protocol::VoiceErrorCode::ToolBusy | protocol::VoiceErrorCode::ToolDeliveryFailed => {
            "the message could not be delivered to the agent"
        }
        protocol::VoiceErrorCode::InvalidAudio => "the microphone audio was rejected",
        protocol::VoiceErrorCode::InvalidRequest
        | protocol::VoiceErrorCode::AlreadyActive
        | protocol::VoiceErrorCode::StaleGeneration
        | protocol::VoiceErrorCode::WrongTarget
        | protocol::VoiceErrorCode::Internal => "an internal voice error occurred",
    };
    let mut message = if payload.retryable {
        format!("Voice ended: {cause}. You can retry.")
    } else {
        format!("Voice ended: {cause}.")
    };
    if let Some(detail) = payload
        .detail
        .as_deref()
        .map(str::trim)
        .filter(|detail| !detail.is_empty())
    {
        message.push_str(&format!(" Provider said: “{detail}”"));
    }
    message
}

/// Roughly fifteen seconds of 20ms Opus packets. The cap bounds a session that
/// never reaches acceptance. It drops the newest packet rather than the oldest
/// because the head of the queue holds the words spoken first, which is exactly
/// what capture-first exists to keep.
const VOICE_UPLINK_QUEUE_LIMIT: usize = 750;

thread_local! {
    static VOICE_UPLINK_QUEUE: RefCell<VecDeque<bridge::VoiceOpusPacketEvent>> =
        const { RefCell::new(VecDeque::new()) };
    static VOICE_UPLINK_PUMP_RUNNING: Cell<bool> = const { Cell::new(false) };
}

fn clear_voice_uplink_queue() {
    VOICE_UPLINK_QUEUE.with(|queue| queue.borrow_mut().clear());
}

/// Captured input goes through one FIFO drained by a single task. The server
/// treats any packet whose `first_media_seq` is below what it has already seen
/// as a duplicate and discards it, so a flush that raced live capture would
/// silently lose the buffered opening words. One drain is what makes the order
/// on the wire the order the microphone produced.
fn enqueue_voice_uplink(state: &AppState, event: bridge::VoiceOpusPacketEvent) {
    let queued = VOICE_UPLINK_QUEUE.with(|queue| {
        let mut queue = queue.borrow_mut();
        if queue.len() >= VOICE_UPLINK_QUEUE_LIMIT {
            return false;
        }
        queue.push_back(event);
        true
    });
    if !queued {
        log::warn!("voice uplink queue is full; dropping a captured packet");
        return;
    }
    drain_voice_uplink_queue(state);
}

fn drain_voice_uplink_queue(state: &AppState) {
    if VOICE_UPLINK_PUMP_RUNNING.with(|running| running.replace(true)) {
        return;
    }
    let state = state.clone();
    spawn_local(async move {
        loop {
            let Some(event) = VOICE_UPLINK_QUEUE.with(|queue| queue.borrow_mut().pop_front())
            else {
                VOICE_UPLINK_PUMP_RUNNING.with(|running| running.set(false));
                // A released hold ends the turn here rather than at the release
                // itself: `voice_input_end` ahead of the audio it follows makes
                // the server reject that audio outright.
                if state.voice_finish_pending.get_untracked()
                    && matches!(state.voice_ui.get_untracked(), VoiceUiState::Active { .. })
                {
                    state.voice_finish_pending.set(false);
                    finish_dictation(state.clone());
                }
                return;
            };
            match route_voice_uplink(&state.voice_ui.get_untracked(), event) {
                VoiceUplinkRouting::Send(prepared) => {
                    let _ = send_prepared_voice_uplink_with(
                        prepared,
                        |host_id, stream, payload, opus| async move {
                            send_binary_frame(
                                &host_id,
                                stream,
                                FrameKind::VoiceAudio,
                                &payload,
                                &opus,
                            )
                            .await
                        },
                    )
                    .await;
                }
                VoiceUplinkRouting::Hold(event) => {
                    VOICE_UPLINK_QUEUE.with(|queue| queue.borrow_mut().push_front(event));
                    VOICE_UPLINK_PUMP_RUNNING.with(|running| running.set(false));
                    return;
                }
                VoiceUplinkRouting::Drop => {}
            }
        }
    });
}

fn handle_voice_opus_packet(packet_state: &AppState, event: bridge::VoiceOpusPacketEvent) {
    enqueue_voice_uplink(packet_state, event);
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

/// Downlink audio packets and playback flushes ride one ordered FIFO drained
/// by a single task. Spawning an IPC task per packet gave no ordering
/// guarantee — a reordered pair garbles the stateful Opus decoder — and a
/// flush racing ahead of queued packets could eat the head of the response
/// that follows an interrupt. A Flush entering the queue drops every packet
/// queued before it: by FIFO order those all predate the interrupt.
enum QueuedMediaItem {
    Packet {
        generation: u64,
        media_seq: u64,
        timestamp_samples_48k: u64,
        opus: Vec<u8>,
    },
    Flush {
        generation: u64,
    },
}

thread_local! {
    static VOICE_MEDIA_PUSH_QUEUE: RefCell<VecDeque<QueuedMediaItem>> =
        const { RefCell::new(VecDeque::new()) };
    static VOICE_MEDIA_PUSHER_RUNNING: Cell<bool> = const { Cell::new(false) };
}

fn clear_media_push_queue() {
    VOICE_MEDIA_PUSH_QUEUE.with(|queue| queue.borrow_mut().clear());
}

fn enqueue_media_item(state: &AppState, item: QueuedMediaItem) {
    VOICE_MEDIA_PUSH_QUEUE.with(|queue| {
        let mut queue = queue.borrow_mut();
        if matches!(item, QueuedMediaItem::Flush { .. }) {
            queue.clear();
        }
        queue.push_back(item);
    });
    if VOICE_MEDIA_PUSHER_RUNNING.with(|running| running.replace(true)) {
        return;
    }
    let state = state.clone();
    spawn_local(async move {
        loop {
            let Some(item) = VOICE_MEDIA_PUSH_QUEUE.with(|queue| queue.borrow_mut().pop_front())
            else {
                VOICE_MEDIA_PUSHER_RUNNING.with(|running| running.set(false));
                return;
            };
            match item {
                QueuedMediaItem::Packet {
                    generation,
                    media_seq,
                    timestamp_samples_48k,
                    opus,
                } => {
                    if let Err(error) = bridge::voice_media_push_output(
                        generation,
                        media_seq,
                        timestamp_samples_48k,
                        &opus,
                    )
                    .await
                    {
                        log::warn!("voice output packet {media_seq} not played: {error}");
                        state.voice_ui.update(|current| {
                            if let VoiceUiState::Active {
                                generation: active_generation,
                                dropped_output_packets,
                                ..
                            } = current
                                && *active_generation == generation
                            {
                                *dropped_output_packets = dropped_output_packets.saturating_add(1);
                            }
                        });
                    }
                }
                QueuedMediaItem::Flush { generation } => {
                    if let Err(error) = bridge::voice_media_flush_output(generation).await {
                        log::warn!("voice playback flush failed: {error}");
                    }
                }
            }
        }
    });
}

#[derive(Debug, PartialEq, Eq)]
enum VoiceDownlinkRejection {
    Inactive,
    Generation,
    Sequence,
    InputOnly,
}

fn admit_voice_downlink(
    current: &mut VoiceUiState,
    payload: &protocol::VoiceAudioPayload,
) -> Result<u64, (u64, VoiceDownlinkRejection)> {
    match current {
        VoiceUiState::Active {
            generation,
            session_id,
            request,
            next_output_media_seq,
            dropped_output_packets,
            ..
        } if *generation == payload.generation && session_id.0 == payload.session_id.0 => {
            if request.mode() == protocol::VoiceMode::Dictation {
                return Err((*generation, VoiceDownlinkRejection::InputOnly));
            }
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
        enqueue_media_item(
            frame_state,
            QueuedMediaItem::Packet {
                generation,
                media_seq,
                timestamp_samples_48k,
                opus: packet,
            },
        );
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

    let recovery_state = state.clone();
    listeners.push(
        bridge::listen_host_recovery(move |event| {
            if !event.connected
                && matches!(recovery_state.voice_ui.get_untracked(),
            VoiceUiState::Starting { ref host_id, .. } | VoiceUiState::Active { ref host_id, .. }
                if host_id == &event.host_id)
            {
                stop(
                    recovery_state.clone(),
                    protocol::VoiceStopReason::TransportLost,
                );
            }
        })
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

/// The live-session surface: connecting, the transcript with its phase pulse,
/// and the two session actions. Idle is deliberately absent — starting voice
/// is offered by [`VoiceComposerButton`] in the composer row, so this renders
/// only once a session exists.
/// The status word and orb animation for a live phase. "Thinking" covers the
/// agent-tool wait, matching what the user perceives: the assistant heard
/// them and is off working.
fn phase_presentation(phase: protocol::VoiceSessionState) -> (&'static str, &'static str) {
    match phase {
        protocol::VoiceSessionState::Starting => ("Connecting", "voice-orb--starting"),
        protocol::VoiceSessionState::Listening => ("Listening", "voice-orb--listening"),
        protocol::VoiceSessionState::AgentWorking => ("Thinking", "voice-orb--thinking"),
        protocol::VoiceSessionState::Speaking => ("Speaking", "voice-orb--speaking"),
        protocol::VoiceSessionState::Interrupting => ("Interrupting", "voice-orb--speaking"),
        protocol::VoiceSessionState::Ending | protocol::VoiceSessionState::Ended => {
            ("Ending", "voice-orb--starting")
        }
    }
}

fn lane_row(label: &'static str, text: Option<String>, test_id: &'static str) -> impl IntoView {
    view! {
        <div class="voice-lane" data-testid=test_id>
            <span class="voice-lane-label">{label}</span>
            <span class="voice-lane-text">{text.unwrap_or_else(|| "—".into())}</span>
        </div>
    }
}

#[component]
fn VoiceSessionControls(state: AppState) -> impl IntoView {
    move || {
        let stop_state = state.clone();
        let interrupt_state = state.clone();
        let dismiss_state = state.clone();
        let collapsed = state.voice_band_collapsed;
        let in_chat = state.voice_transcript_in_chat;
        match state.voice_ui.get() {
            VoiceUiState::Idle => ().into_any(),
            VoiceUiState::Starting { request, .. } => view! {
                <div class="voice-band voice-band--starting">
                    <span class="voice-orb voice-orb--starting" aria-hidden="true"></span>
                    <span class="voice-band-status">{if request.mode() == protocol::VoiceMode::Dictation { "Connecting dictation…" } else { "Connecting voice…" }}</span>
                </div>
            }
            .into_any(),
            VoiceUiState::Active {
                request: protocol::VoiceAcceptedRequest::Dictation { .. },
                dictation: Some(dictation),
                ..
            } => {
                let finish_state = state.clone();
                let cancel_state = state.clone();
                let status = if dictation.finishing {
                    "Finishing dictation…"
                } else {
                    "Listening for dictation…"
                };
                view! {
                    <div class="voice-band voice-band--dictation" data-testid="dictation-session">
                        <span class="voice-orb voice-orb--listening" aria-hidden="true"></span>
                        <span class="voice-band-status">{status}</span>
                        <span class="voice-band-ticker" data-testid="dictation-partial">
                            {dictation.partial.unwrap_or_default()}
                        </span>
                        <button
                            class="chat-send-btn voice-action"
                            data-testid="dictation-finish"
                            disabled=dictation.finishing
                            on:click=move |_| finish_dictation(finish_state.clone())
                        >
                            "Finish"
                        </button>
                        <button
                            class="chat-send-btn voice-action"
                            data-testid="dictation-cancel"
                            on:click=move |_| {
                                stop(cancel_state.clone(), protocol::VoiceStopReason::UserExited)
                            }
                        >
                            "Cancel"
                        </button>
                    </div>
                }
                .into_any()
            }
            VoiceUiState::Active {
                request: protocol::VoiceAcceptedRequest::Conversation { .. },
                state: phase,
                lanes,
                ..
            } => {
                let (status, orb_class) = phase_presentation(phase);
                let agent_lane = lanes.agent.clone().or_else(|| {
                    (phase == protocol::VoiceSessionState::AgentWorking)
                        .then(|| "working…".to_string())
                });
                if collapsed.get() {
                    let ticker = lanes.you.clone().unwrap_or_default();
                    view! {
                        <div class="voice-band voice-band--collapsed" data-testid="voice-band-collapsed">
                            <span class=format!("voice-orb voice-orb--mini {orb_class}") aria-hidden="true"></span>
                            <span class="voice-band-status">{status}</span>
                            <span class="voice-band-ticker">{ticker}</span>
                            <button
                                class="chat-send-btn voice-action"
                                data-testid="voice-band-expand"
                                aria-label="Expand voice panel"
                                title="Expand voice panel"
                                on:click=move |_| collapsed.set(false)
                            >
                                "⌃"
                            </button>
                            <button
                                class="chat-send-btn voice-action"
                                on:click=move |_| {
                                    stop(stop_state.clone(), protocol::VoiceStopReason::UserExited)
                                }
                            >
                                "End"
                            </button>
                        </div>
                    }
                    .into_any()
                } else {
                    view! {
                        <div class="voice-band voice-band--expanded" data-testid="voice-band-expanded">
                            <div class="voice-band-orb-zone">
                                <span class=format!("voice-orb {orb_class}") aria-hidden="true"></span>
                                <span class="voice-band-status">{status}</span>
                            </div>
                            <div class="voice-band-lanes">
                                {lane_row("You", lanes.you.clone(), "voice-lane-you")}
                                {lane_row("Nova", lanes.nova.clone(), "voice-lane-nova")}
                                {lane_row("Agent", agent_lane, "voice-lane-agent")}
                            </div>
                            <div class="voice-band-controls">
                                <button
                                    class="chat-send-btn voice-action"
                                    data-testid="voice-band-collapse"
                                    aria-label="Collapse voice panel"
                                    title="Collapse voice panel"
                                    on:click=move |_| collapsed.set(true)
                                >
                                    "⌄"
                                </button>
                                <button
                                    class="chat-send-btn voice-action"
                                    on:click=move |_| interrupt(interrupt_state.clone())
                                >
                                    "Interrupt"
                                </button>
                                <button
                                    class="chat-send-btn voice-action"
                                    on:click=move |_| {
                                        stop(
                                            stop_state.clone(),
                                            protocol::VoiceStopReason::UserExited,
                                        )
                                    }
                                >
                                    "End"
                                </button>
                                <label class="voice-band-toggle" title="Also append finalized voice transcripts to the agent chat">
                                    <input
                                        type="checkbox"
                                        prop:checked=move || in_chat.get()
                                        on:change=move |_| in_chat.update(|value| *value = !*value)
                                    />
                                    "Transcript in chat"
                                </label>
                            </div>
                        </div>
                    }
                    .into_any()
                }
            }
            VoiceUiState::Active { .. } => ().into_any(),
            VoiceUiState::Failed { error, .. } => view! {
                <div class="voice-band voice-band--failed">
                    <span class="voice-orb voice-orb--failed" aria-hidden="true"></span>
                    <span class="voice-error" data-testid="voice-error">{error}</span>
                    <button
                        class="chat-send-btn voice-action"
                        on:click=move |_| dismiss_state.voice_ui.set(VoiceUiState::Idle)
                    >
                        "Dismiss"
                    </button>
                </div>
            }
            .into_any(),
        }
    }
}

/// Below this, a press is a tap that latches the session open; at or above it
/// the press is a hold whose release ends the turn. Set by feel: long enough
/// that an ordinary click never starts a hold, short enough that a deliberate
/// press-and-speak always does.
const VOICE_HOLD_THRESHOLD_MS: f64 = 350.0;

const VOICE_OFFER_UNAVAILABLE: &str = "Speech is unavailable in this chat";

/// Whether each speech mode can start for one composer. A mode holds `None`
/// when it is startable and `Some(reason)` when it is not, so the mode menu can
/// say *why* a mode is out rather than offering a dead entry the way the old
/// disabled `<option>` did.
#[derive(Clone, PartialEq, Eq, Debug)]
struct ComposerVoiceOffer {
    host: Option<String>,
    conversation_block: Option<&'static str>,
    dictation_block: Option<&'static str>,
}

impl ComposerVoiceOffer {
    fn blocked() -> Self {
        Self {
            host: None,
            conversation_block: Some(VOICE_OFFER_UNAVAILABLE),
            dictation_block: Some(VOICE_OFFER_UNAVAILABLE),
        }
    }

    fn conversation(&self) -> bool {
        self.conversation_block.is_none()
    }

    fn dictation(&self) -> bool {
        self.dictation_block.is_none()
    }

    fn allows(&self, mode: protocol::VoiceMode) -> bool {
        match mode {
            protocol::VoiceMode::Conversation => self.conversation(),
            protocol::VoiceMode::Dictation => self.dictation(),
        }
    }

    fn any(&self) -> bool {
        self.conversation() || self.dictation()
    }

    fn block(&self, mode: protocol::VoiceMode) -> Option<&'static str> {
        match mode {
            protocol::VoiceMode::Conversation => self.conversation_block,
            protocol::VoiceMode::Dictation => self.dictation_block,
        }
    }

    /// The mode the mic actually starts: the remembered choice when it can run,
    /// otherwise whichever mode can. `None` when neither is startable.
    fn effective(&self, chosen: protocol::VoiceMode) -> Option<protocol::VoiceMode> {
        if self.allows(chosen) {
            return Some(chosen);
        }
        [
            protocol::VoiceMode::Dictation,
            protocol::VoiceMode::Conversation,
        ]
        .into_iter()
        .find(|mode| self.allows(*mode))
    }
}

fn composer_voice_availability(
    state: &AppState,
    agent_ref: Signal<Option<ActiveAgentRef>>,
    composer: ComposerHandle,
) -> Memo<ComposerVoiceOffer> {
    let state = state.clone();
    Memo::new(move |_| {
        #[cfg(target_arch = "wasm32")]
        let owns_active_composer = state.composer().text == composer.text;
        #[cfg(not(target_arch = "wasm32"))]
        let owns_active_composer = state.composer_untracked().text == composer.text;
        if !owns_active_composer || !state.native_voice_supported.get() {
            return ComposerVoiceOffer::blocked();
        }
        let active = state.active_agent.get();
        let requested_agent = agent_ref.get();
        if requested_agent != active {
            return ComposerVoiceOffer::blocked();
        }
        let host = requested_agent
            .as_ref()
            .map(|agent| agent.host_id.clone())
            .or_else(|| state.chat_context_host_id());
        let Some(host) = host else {
            return ComposerVoiceOffer::blocked();
        };
        let settings = state
            .host_settings_by_host
            .with(|settings| settings.get(&host).cloned());
        let capabilities = state
            .voice_capabilities_by_host
            .with(|capabilities| capabilities.get(&host).cloned());
        let conversation_block = if !settings.as_ref().is_some_and(|value| value.voice.enabled) {
            Some("Voice is turned off for this host")
        } else if !capabilities
            .as_ref()
            .is_some_and(|value| value.nova_available)
        {
            Some("Nova is not configured for this host")
        } else if requested_agent.is_none()
            || !target_with_resolution_tracked(&state).0.target_resolvable
        {
            Some("Open a running agent to talk with Nova")
        } else {
            None
        };
        let dictation_block = if !settings
            .as_ref()
            .is_some_and(|value| value.voice.dictation_enabled)
        {
            Some("Dictation is turned off for this host")
        } else if !capabilities
            .as_ref()
            .is_some_and(|value| value.dictation_available)
        {
            Some("Amazon Transcribe is not configured for this host")
        } else {
            None
        };
        ComposerVoiceOffer {
            host: Some(host),
            conversation_block,
            dictation_block,
        }
    })
}

/// Dictation is the default because it is the mode that can nearly always run
/// (it needs no agent and no resolvable target) and the one that cannot act on
/// your behalf — it only fills the composer, which you still read and send.
pub fn initial_voice_mode() -> protocol::VoiceMode {
    #[cfg(target_arch = "wasm32")]
    if let Some(storage) =
        web_sys::window().and_then(|window| window.local_storage().ok().flatten())
        && storage
            .get_item("tyde.voice.mode")
            .ok()
            .flatten()
            .as_deref()
            == Some("conversation")
    {
        return protocol::VoiceMode::Conversation;
    }
    protocol::VoiceMode::Dictation
}

pub fn voice_mode_value(mode: protocol::VoiceMode) -> &'static str {
    match mode {
        protocol::VoiceMode::Conversation => "conversation",
        protocol::VoiceMode::Dictation => "dictation",
    }
}

pub fn voice_mode_label(mode: protocol::VoiceMode) -> &'static str {
    match mode {
        protocol::VoiceMode::Conversation => "Talk with Nova",
        protocol::VoiceMode::Dictation => "Dictate to composer",
    }
}

fn voice_mode_hint(mode: protocol::VoiceMode) -> &'static str {
    match mode {
        protocol::VoiceMode::Conversation => "Hold a spoken conversation with Nova",
        protocol::VoiceMode::Dictation => "Speak to fill the composer, then edit before sending",
    }
}

/// A distinct glyph per mode so the button reads as its current mode at a
/// glance — a headset for the conversation, a mic for dictation — without the
/// text label the mode select used to spend composer width on.
fn voice_mode_icon(mode: protocol::VoiceMode) -> AnyView {
    match mode {
        protocol::VoiceMode::Dictation => view! {
            <svg
                class="voice-mic-icon"
                viewBox="0 0 24 24"
                width="16"
                height="16"
                fill="currentColor"
                aria-hidden="true"
            >
                <path d="M12 14a3 3 0 0 0 3-3V5a3 3 0 0 0-6 0v6a3 3 0 0 0 3 3z" />
                <path d="M19 11a1 1 0 1 0-2 0 5 5 0 0 1-10 0 1 1 0 1 0-2 0 7 7 0 0 0 6 6.92V20H8.5a1 1 0 1 0 0 2h7a1 1 0 1 0 0-2H13v-2.08A7 7 0 0 0 19 11z" />
            </svg>
        }
        .into_any(),
        protocol::VoiceMode::Conversation => view! {
            <svg
                class="voice-mic-icon"
                viewBox="0 0 24 24"
                width="16"
                height="16"
                fill="currentColor"
                aria-hidden="true"
            >
                <path d="M12 3a8 8 0 0 0-8 8v1a1 1 0 0 0 2 0v-1a6 6 0 0 1 12 0v1a1 1 0 0 0 2 0v-1a8 8 0 0 0-8-8z" />
                <path d="M4 13a2 2 0 0 1 2 2v3a2 2 0 1 1-4 0v-3a2 2 0 0 1 2-2z" />
                <path d="M20 13a2 2 0 0 1 2 2v3a2 2 0 1 1-4 0v-3a2 2 0 0 1 2-2z" />
            </svg>
        }
        .into_any(),
    }
}

fn remember_voice_mode(mode: protocol::VoiceMode) {
    #[cfg(not(target_arch = "wasm32"))]
    let _ = mode;
    #[cfg(target_arch = "wasm32")]
    if let Some(storage) =
        web_sys::window().and_then(|window| window.local_storage().ok().flatten())
    {
        let _ = storage.set_item("tyde.voice.mode", voice_mode_value(mode));
    }
}

/// Start-voice affordance, rendered in the composer's button row beside Send.
///
/// It lives here rather than in a floating overlay because the overlay was
/// `position: fixed` at the bottom-centre of the viewport — exactly where the
/// composer sits — so it painted on top of the textarea and read as a button
/// inside the text input.
///
/// It is a split button rather than a mode `<select>` plus a mic: the mode is a
/// sticky preference you set once, so it belongs behind the caret with the
/// other composer menus, not in 150px of permanent composer width. The caret
/// lists both modes always — a mode that cannot start stays visible but
/// disabled and says why, which a disabled `<option>` could not.
#[component]
pub fn VoiceComposerButton(
    agent_ref: Signal<Option<ActiveAgentRef>>,
    composer: ComposerHandle,
) -> impl IntoView {
    let state = expect_context::<AppState>();
    let offer = composer_voice_availability(&state, agent_ref, composer.clone());
    // A press is held while the pointer is still down. The button has to stay
    // mounted for that whole time or the release would land on nothing, so the
    // idle-only rule below is relaxed for the duration of a hold.
    let holding = RwSignal::new(false);
    let hold_started_at = StoredValue::new(0.0_f64);
    let idle_state = state.clone();
    let show = Memo::new(move |_| {
        offer.get().any()
            && (matches!(idle_state.voice_ui.get(), VoiceUiState::Idle) || holding.get())
    });
    let mode_state = state.clone();
    let effective_mode =
        Memo::new(move |_| offer.get().effective(mode_state.voice_mode_choice.get()));
    let menu_open = RwSignal::new(false);
    let choice_state = StoredValue::new(state.clone());
    let gesture_state = StoredValue::new(state.clone());
    let start_state = StoredValue::new((state, composer));
    let start_effective = move || {
        let Some(mode) = effective_mode.get_untracked() else {
            return;
        };
        start_state.with_value(|(state, composer)| match mode {
            protocol::VoiceMode::Dictation => {
                if let Some(host) = offer.get_untracked().host {
                    start_dictation(state.clone(), host, composer.text.clone());
                }
            }
            protocol::VoiceMode::Conversation => start_conversation(state.clone()),
        })
    };
    // Hold-to-talk belongs to dictation, which is a burst: one utterance into
    // the composer, with the release marking its end. A conversation is a
    // session you enter and stay in, so it keeps plain click-to-start.
    let hold_applies =
        move || effective_mode.get_untracked() == Some(protocol::VoiceMode::Dictation);
    let on_press = move |event: web_sys::PointerEvent| {
        if !hold_applies()
            || !gesture_state
                .with_value(|state| matches!(state.voice_ui.get_untracked(), VoiceUiState::Idle))
        {
            return;
        }
        // Keeps the release on this button even if the pointer drifts off it
        // while the user is talking.
        if let Some(target) = event
            .target()
            .and_then(|target| target.dyn_into::<web_sys::Element>().ok())
        {
            let _ = target.set_pointer_capture(event.pointer_id());
        }
        hold_started_at.set_value(js_sys::Date::now());
        holding.set(true);
        start_effective();
    };
    let on_release = move |_: web_sys::PointerEvent| {
        if !holding.get_untracked() {
            return;
        }
        holding.set(false);
        // Anything shorter than a deliberate hold is a tap, which latches the
        // session open so it can be ended from the session bar. Keyboard users
        // reach the same latch through the click handler, which never sees a
        // pointer sequence at all.
        if js_sys::Date::now() - hold_started_at.get_value() < VOICE_HOLD_THRESHOLD_MS {
            return;
        }
        gesture_state.with_value(|state| request_dictation_finish(state.clone()));
    };
    let mode_item = move |mode: protocol::VoiceMode| {
        let block = Memo::new(move |_| offer.get().block(mode));
        let selected = Memo::new(move |_| effective_mode.get() == Some(mode));
        view! {
            <button
                type="button"
                class="chat-send-menu-item"
                role="menuitemradio"
                aria-checked=move || if selected.get() { "true" } else { "false" }
                data-test=format!("chat-voice-mode-{}", voice_mode_value(mode))
                disabled=move || block.get().is_some()
                title=move || block.get().unwrap_or(voice_mode_hint(mode))
                on:click=move |_| {
                    choice_state.with_value(|state| state.voice_mode_choice.set(mode));
                    remember_voice_mode(mode);
                    menu_open.set(false);
                }
            >
                <span class="chat-send-menu-label">{voice_mode_label(mode)}</span>
                <span class="chat-send-menu-check" aria-hidden="true">
                    {move || if selected.get() { "✓" } else { "" }}
                </span>
            </button>
        }
    };
    view! {
        <Show when=move || show.get()>
            <div class="chat-send-split chat-voice-split" role="group" aria-label="Speech actions">
                <button
                    type="button"
                    class="chat-send-btn chat-voice-btn chat-send-split-primary"
                    data-test="chat-voice-start"
                    data-voice-mode=move || {
                        effective_mode.get().map(voice_mode_value).unwrap_or("none")
                    }
                    class:chat-voice-btn--holding=move || holding.get()
                    data-holding=move || if holding.get() { "true" } else { "false" }
                    aria-label=move || {
                        effective_mode.get().map(voice_mode_label).unwrap_or("Start speech")
                    }
                    title=move || match effective_mode.get() {
                        Some(protocol::VoiceMode::Dictation) => {
                            "Hold to dictate, or tap to keep dictating"
                        }
                        Some(mode) => voice_mode_label(mode),
                        None => "Start speech",
                    }
                    on:pointerdown=on_press
                    on:pointerup=on_release
                    on:pointercancel=on_release
                    on:click=move |_| {
                        // A pointer press has already started the session, so
                        // the click that follows it must not start a second
                        // one. Only a keyboard activation reaches this idle.
                        if gesture_state.with_value(|state| {
                            matches!(state.voice_ui.get_untracked(), VoiceUiState::Idle)
                        }) {
                            start_effective();
                        }
                    }
                >
                    {move || effective_mode.get().map(voice_mode_icon)}
                </button>
                <button
                    type="button"
                    class="chat-send-btn chat-send-split-toggle"
                    data-test="chat-voice-mode-toggle"
                    aria-haspopup="menu"
                    aria-expanded=move || if menu_open.get() { "true" } else { "false" }
                    aria-label="Choose speech mode"
                    title="Choose speech mode"
                    on:click=move |_| menu_open.update(|open| *open = !*open)
                >
                    <span aria-hidden="true">"⌄"</span>
                </button>
                <Show when=move || menu_open.get()>
                    <div
                        class="chat-send-menu-backdrop"
                        on:click=move |_| menu_open.set(false)
                    ></div>
                    <div
                        class="chat-send-menu"
                        role="menu"
                        aria-label="Speech mode"
                        data-test="chat-voice-mode-menu"
                    >
                        {mode_item(protocol::VoiceMode::Dictation)}
                        {mode_item(protocol::VoiceMode::Conversation)}
                    </div>
                </Show>
            </div>
        </Show>
    }
}

/// Live-session strip, rendered in the composer stack above the input row (with
/// the other composer notices) so it shifts the textarea down instead of
/// covering it.
#[component]
pub fn VoiceComposerBar(
    agent_ref: Signal<Option<ActiveAgentRef>>,
    composer: ComposerHandle,
) -> impl IntoView {
    let state = expect_context::<AppState>();
    let offer = composer_voice_availability(&state, agent_ref, composer.clone());
    let session_state = state.clone();
    let in_session = Memo::new(move |_| match session_state.voice_ui.get() {
        VoiceUiState::Starting {
            dictation: Some(dictation),
            ..
        }
        | VoiceUiState::Active {
            dictation: Some(dictation),
            ..
        } => dictation.composer_text == composer.text,
        VoiceUiState::Failed {
            composer_text: Some(origin),
            ..
        } => origin == composer.text,
        VoiceUiState::Idle => false,
        _ => offer.get().conversation(),
    });
    let render_state = StoredValue::new(state);
    view! {
        <Show when=move || in_session.get()>
            <div class="chat-voice-bar" data-testid="voice-session-bar">
                <VoiceSessionControls state=render_state.get_value() />
            </div>
        </Show>
    }
}

/// Root-mounted and renders nothing: it owns the native media listeners and the
/// stop-on-target-change effect for the whole app. Every visible voice control
/// lives in the composer ([`VoiceComposerButton`], [`VoiceComposerBar`]).
#[component]
pub fn VoiceRuntime() -> impl IntoView {
    let state = expect_context::<AppState>();
    install_media_listeners(state.clone());
    let previous = StoredValue::new(None::<ActiveAgentRef>);
    let switch_state = state;
    Effect::new(move |_| {
        let active = switch_state.active_agent.get();
        let conversation_active = matches!(
            switch_state.voice_ui.get_untracked(),
            VoiceUiState::Starting {
                request: protocol::VoiceRequest::Conversation { .. },
                ..
            } | VoiceUiState::Active {
                request: protocol::VoiceAcceptedRequest::Conversation { .. },
                ..
            }
        );
        if conversation_active && previous.get_value().is_some() && previous.get_value() != active {
            stop(
                switch_state.clone(),
                protocol::VoiceStopReason::TargetChanged,
            );
        }
        previous.set_value(active);
    });
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
            "__tyde_voice_test_send_host_frame_log",
            "__tyde_voice_test_push_output_args",
            "__tyde_voice_test_media_start_args",
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
            window.__tyde_voice_test_send_host_frame_log = [];
            window.__tyde_voice_test_push_output_args = undefined;
            window.__tyde_voice_test_media_start_args = undefined;
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
                        if (command === "voice_media_start") {
                            window.__tyde_voice_test_media_start_args = args;
                            return Promise.resolve(null);
                        }
                        if (command === "send_host_frame") {
                            window.__tyde_voice_test_send_host_frame_args = args;
                            window.__tyde_voice_test_send_host_frame_log.push(args);
                            return Promise.resolve(null);
                        }
                        if (command === "send_host_line") {
                            // Control frames travel on a different command from
                            // binary audio. Both land in one ordered log so a
                            // test can assert how they interleave.
                            window.__tyde_voice_test_send_host_frame_log.push({
                                envelope: args.line
                            });
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
            request: protocol::VoiceAcceptedRequest::Conversation {
                target: protocol::VoiceTarget {
                    agent_id: AgentId("voice-agent".to_owned()),
                    instance_stream: StreamPath("/agent/voice-agent/instance".to_owned()),
                },
                uplink: protocol::VoiceAudioFormat::opus(48_000),
                downlink: protocol::VoiceAudioFormat::opus(24_000),
            },
            dictation: None,
            state: protocol::VoiceSessionState::Listening,
            lanes: VoiceLanes::default(),
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
            view! { <VoiceRuntime /> }
        });
        wait_for_voice_listener_attempts(5).await;
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

    fn logged_envelopes() -> Vec<protocol::Envelope> {
        let log = js_sys::Array::from(&captured_invoke("__tyde_voice_test_send_host_frame_log"));
        (0..log.length())
            .map(|index| {
                #[derive(serde::Deserialize)]
                #[serde(rename_all = "camelCase")]
                struct Invocation {
                    envelope: String,
                }
                let invocation: Invocation = serde_wasm_bindgen::from_value(log.get(index))
                    .expect("decode send_host_frame invocation");
                serde_json::from_str(&invocation.envelope).expect("decode envelope")
            })
            .collect()
    }

    fn uplink_log() -> Vec<protocol::VoiceAudioPayload> {
        logged_envelopes()
            .into_iter()
            .filter(|envelope| envelope.kind == FrameKind::VoiceAudio)
            .map(|envelope| envelope.parse_payload().expect("decode audio payload"))
            .collect()
    }

    async fn sleep_ms(ms: i32) {
        let promise = js_sys::Promise::new(&mut |resolve, _| {
            web_sys::window()
                .expect("window")
                .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms)
                .expect("schedule timeout");
        });
        let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
    }

    fn dispatch_pointer(element: &web_sys::Element, kind: &str) {
        let init = web_sys::PointerEventInit::new();
        init.set_bubbles(true);
        init.set_pointer_id(1);
        let event = web_sys::PointerEvent::new_with_event_init_dict(kind, &init)
            .expect("construct pointer event");
        element
            .dispatch_event(&event)
            .expect("dispatch pointer event");
    }

    async fn wait_for_uplinks(expected: usize) -> Vec<protocol::VoiceAudioPayload> {
        for _ in 0..16 {
            let sent = uplink_log();
            if sent.len() >= expected {
                return sent;
            }
            next_tick().await;
        }
        uplink_log()
    }

    /// **A held press dictates, and its release ends the turn behind its audio.**
    ///
    /// The gesture has to hold together across the whole session: the press
    /// opens the microphone before the host has accepted anything, and the
    /// release must not send `voice_input_end` until the audio it captured has
    /// gone out, because the server rejects input that arrives after the end.
    #[wasm_bindgen_test]
    async fn holding_the_mic_dictates_and_release_ends_the_turn_behind_its_audio() {
        #[derive(serde::Serialize)]
        #[serde(rename_all = "camelCase")]
        struct UplinkEvent {
            generation: u64,
            media_seq: u64,
            timestamp_samples_48k: u64,
            opus: Vec<u8>,
        }

        let _stub = install_voice_listener_stub(None, None);
        let container = container();
        let state = AppState::new();
        open_voice_gate(&state);
        state.host_settings_by_host.update(|settings| {
            settings.get_mut("local").unwrap().voice.dictation_enabled = true;
            settings.get_mut("local").unwrap().voice.dictation_region =
                Some("us-west-2".to_owned());
        });
        state.voice_capabilities_by_host.update(|capabilities| {
            capabilities.insert(
                "local".to_owned(),
                protocol::VoiceCapabilitiesPayload::for_connection(true, true, true),
            );
        });
        state.voice_mode_choice.set(protocol::VoiceMode::Dictation);
        let render_state = state.clone();
        let mount = mount_to(container.clone(), move || {
            let state = render_state.clone();
            provide_context(state.clone());
            let agent_ref = Signal::derive(move || state.active_agent.get());
            let composer = render_state.composer();
            view! {
                <VoiceRuntime />
                <VoiceComposerButton agent_ref=agent_ref composer=composer />
            }
        });
        wait_for_voice_listener_attempts(5).await;
        next_tick().await;

        let mic = start_button(&container).expect("the mic must be offered");
        dispatch_pointer(&mic, "pointerdown");
        next_tick().await;
        next_tick().await;

        // The press alone opens the microphone: capture must not wait for the
        // host, which is the whole point of holding to talk.
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct MediaStart {
            input_only: bool,
            pending_acceptance: bool,
        }
        let media_start: MediaStart =
            serde_wasm_bindgen::from_value(captured_invoke("__tyde_voice_test_media_start_args"))
                .expect("the press must start capture");
        assert!(media_start.input_only);
        assert!(
            media_start.pending_acceptance,
            "a held press captures before the host accepts"
        );
        assert!(
            mic.get_attribute("data-holding").as_deref() == Some("true"),
            "the mic must show that it is recording for the length of the press"
        );

        let generation = state.voice_generation.get_untracked();
        // Speak while the provider stream is still opening — the realistic
        // hold: the press is over long before a Transcribe stream is live.
        for seq in 0..2u64 {
            dispatch_tauri_voice_event(
                "__tyde_voice_test_packet_dispatch",
                "tyde://voice-opus-packet",
                UplinkEvent {
                    generation,
                    media_seq: seq,
                    timestamp_samples_48k: seq * 960,
                    opus: vec![seq as u8 + 1],
                },
            );
        }

        // Past the tap threshold, so letting go ends the turn rather than
        // latching the session open.
        sleep_ms(450).await;
        dispatch_pointer(&mic, "pointerup");
        next_tick().await;
        next_tick().await;
        assert!(
            logged_envelopes()
                .iter()
                .all(|envelope| envelope.kind != FrameKind::VoiceInputEnd),
            "a release cannot end a turn the host has not accepted yet"
        );
        assert!(
            uplink_log().is_empty(),
            "audio held from before acceptance must not have been sent yet"
        );

        let session = protocol::VoiceSessionId("hold-session".to_owned());
        let accepted = protocol::Envelope::from_payload(
            StreamPath(format!("/voice/{}", session.0)),
            FrameKind::VoiceAccepted,
            0,
            &protocol::VoiceAcceptedPayload {
                session_id: session.clone(),
                generation,
                request: protocol::VoiceAcceptedRequest::Dictation {
                    uplink: protocol::VoiceAudioFormat::opus(48_000),
                },
            },
        )
        .expect("build acceptance envelope");
        handle_control(&state, "local", &accepted);
        for _ in 0..24 {
            if logged_envelopes()
                .iter()
                .any(|envelope| envelope.kind == FrameKind::VoiceInputEnd)
            {
                break;
            }
            next_tick().await;
        }

        // Everything the hold captured reaches the host, and the turn ends
        // behind it: `voice_input_end` sent first would make the server reject
        // exactly the audio the hold existed to capture.
        assert_eq!(
            uplink_log()
                .iter()
                .map(|payload| payload.first_media_seq)
                .collect::<Vec<_>>(),
            vec![0, 1],
            "the held audio must flush in capture order once accepted"
        );
        let kinds: Vec<FrameKind> = logged_envelopes()
            .into_iter()
            .map(|envelope| envelope.kind)
            .collect();
        let last_audio = kinds
            .iter()
            .rposition(|kind| *kind == FrameKind::VoiceAudio)
            .expect("the held audio must be sent");
        let input_end = kinds
            .iter()
            .position(|kind| *kind == FrameKind::VoiceInputEnd)
            .expect("releasing a held press must end the turn");
        assert!(
            last_audio < input_end,
            "voice_input_end must follow every packet it ends, got {kinds:?}"
        );

        drop(mount);
        container.remove();
    }

    /// **Words spoken while the provider stream is still opening must survive.**
    ///
    /// The microphone now opens on the press rather than on acceptance, so
    /// audio exists before there is a session to send it to. It has to be held
    /// and then replayed in capture order: the server discards any packet whose
    /// sequence it has already passed, so a flush that arrived after live
    /// capture would drop precisely the opening words this exists to keep.
    #[wasm_bindgen_test]
    async fn dictation_captured_before_acceptance_flushes_in_capture_order() {
        #[derive(serde::Serialize)]
        #[serde(rename_all = "camelCase")]
        struct UplinkEvent {
            generation: u64,
            media_seq: u64,
            timestamp_samples_48k: u64,
            opus: Vec<u8>,
        }

        let _stub = install_voice_listener_stub(None, None);
        let container = container();
        let state = AppState::new();
        let composer = state.composer();
        state.voice_ui.set(VoiceUiState::Starting {
            generation: 5,
            host_id: "local".to_owned(),
            request: protocol::VoiceRequest::Dictation {
                formats: vec![protocol::VoiceAudioFormat::opus(48_000)],
            },
            dictation: Some(DictationCapture {
                composer_text: composer.text.clone(),
                finalized: String::new(),
                partial: None,
                finishing: false,
            }),
        });
        let render_state = state.clone();
        let mount = mount_to(container.clone(), move || {
            provide_context(render_state.clone());
            view! { <VoiceRuntime /> }
        });
        wait_for_voice_listener_attempts(5).await;
        next_tick().await;

        for seq in 0..3u64 {
            dispatch_tauri_voice_event(
                "__tyde_voice_test_packet_dispatch",
                "tyde://voice-opus-packet",
                UplinkEvent {
                    generation: 5,
                    media_seq: seq,
                    timestamp_samples_48k: seq * 960,
                    opus: vec![seq as u8 + 1],
                },
            );
        }
        next_tick().await;
        next_tick().await;
        assert!(
            uplink_log().is_empty(),
            "audio captured before acceptance must not reach the host early"
        );

        let accepted = protocol::Envelope::from_payload(
            StreamPath("/voice/dictation-session".to_owned()),
            FrameKind::VoiceAccepted,
            0,
            &protocol::VoiceAcceptedPayload {
                session_id: protocol::VoiceSessionId("dictation-session".to_owned()),
                generation: 5,
                request: protocol::VoiceAcceptedRequest::Dictation {
                    uplink: protocol::VoiceAudioFormat::opus(48_000),
                },
            },
        )
        .expect("build acceptance envelope");
        handle_control(&state, "local", &accepted);

        let flushed = wait_for_uplinks(3).await;
        assert_eq!(
            flushed
                .iter()
                .map(|p| p.first_media_seq)
                .collect::<Vec<_>>(),
            vec![0, 1, 2],
            "the held audio must replay in capture order"
        );

        // Audio captured after acceptance queues behind the flush rather than
        // overtaking it, which is what keeps the server from discarding the
        // earlier sequences as duplicates.
        dispatch_tauri_voice_event(
            "__tyde_voice_test_packet_dispatch",
            "tyde://voice-opus-packet",
            UplinkEvent {
                generation: 5,
                media_seq: 3,
                timestamp_samples_48k: 3 * 960,
                opus: vec![9],
            },
        );
        let live = wait_for_uplinks(4).await;
        assert_eq!(
            live.iter().map(|p| p.first_media_seq).collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );

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
            view! { <VoiceRuntime /> }
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
        // Recovery adds a fifth real listener. Defer the final registration
        // and require cleanup of all five, including that late handle.
        let _stub = install_voice_listener_stub(None, Some(5));
        let container = container();
        let state = AppState::new();
        let mount = mount_to(container.clone(), move || {
            provide_context(state.clone());
            view! { <VoiceRuntime /> }
        });

        wait_for_voice_listener_attempts(5).await;
        assert_eq!(
            voice_listener_counter("__tyde_voice_test_active_listeners"),
            5
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
            5,
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

    fn start_button(container: &HtmlElement) -> Option<web_sys::Element> {
        container
            .query_selector("[data-test='chat-voice-start']")
            .unwrap()
    }

    fn session_bar(container: &HtmlElement) -> Option<web_sys::Element> {
        container
            .query_selector("[data-testid='voice-session-bar']")
            .unwrap()
    }

    /// Focus a chat on a started, voice-capable agent — the state in which the
    /// gate is open and the composer may offer voice.
    fn open_voice_gate(state: &AppState) {
        state.host_settings_by_host.update(|settings| {
            let mut host = settings_model::HostSettings::default();
            host.voice.enabled = true;
            settings.insert("local".to_owned(), host);
        });
        state.voice_capabilities_by_host.update(|capabilities| {
            capabilities.insert(
                "local".to_owned(),
                protocol::VoiceCapabilitiesPayload::for_connection(true, false, true),
            );
        });
        state.open_tab(
            TabContent::chat_with_agent(ActiveAgentRef {
                host_id: "local".to_owned(),
                agent_id: AgentId("voice-agent".to_owned()),
            }),
            "Voice agent".to_owned(),
            true,
        );
        state.agents.set(vec![gate_test_agent(true)]);
    }

    /// The composer's two voice surfaces are mutually exclusive and both stay
    /// reactive across repeated state changes: idle offers the start button and
    /// nothing else, a live session replaces it with the session bar, and
    /// returning to idle restores the button.
    #[wasm_bindgen_test]
    async fn composer_voice_surfaces_swap_between_idle_and_session() {
        let container = container();
        let state = AppState::new();
        open_voice_gate(&state);
        let render_state = state.clone();
        let mount = mount_to(container.clone(), move || {
            let state = render_state.clone();
            provide_context(state.clone());
            let agent_ref = Signal::derive(move || state.active_agent.get());
            let composer = render_state.composer();
            view! {
                <VoiceComposerBar agent_ref=agent_ref composer=composer.clone() />
                <VoiceComposerButton agent_ref=agent_ref composer=composer />
            }
        });
        next_tick().await;
        assert!(
            start_button(&container).is_some(),
            "idle must offer the start button"
        );
        assert!(session_bar(&container).is_none());

        state.voice_ui.set(active_voice_state(1));
        next_tick().await;
        assert!(
            start_button(&container).is_none(),
            "a live session must replace the start button, not sit beside it"
        );
        assert!(
            session_bar(&container)
                .expect("session bar")
                .text_content()
                .unwrap()
                .contains("Listening"),
            "the session bar must report the live phase"
        );

        state.voice_ui.set(VoiceUiState::Failed {
            error: "voice unavailable".into(),
            composer_text: None,
        });
        next_tick().await;
        assert!(
            container
                .text_content()
                .unwrap()
                .contains("voice unavailable")
        );
        assert!(start_button(&container).is_none());

        state.voice_ui.set(VoiceUiState::Idle);
        next_tick().await;
        assert!(
            start_button(&container).is_some(),
            "returning to idle must restore the start button"
        );
        assert!(session_bar(&container).is_none());
        drop(mount);
        container.remove();
    }

    /// A server-sent voice_error must leave a visible explanation in the
    /// composer. This pins the fix for the "voice connects then just vanishes"
    /// report: the error frame used to reset the UI straight to Idle,
    /// discarding the error payload, so the session surface disappeared with
    /// no trace of what went wrong.
    #[wasm_bindgen_test]
    async fn voice_error_frame_is_surfaced_and_dismissable() {
        let _stub = install_voice_listener_stub(None, None);
        let container = container();
        let state = AppState::new();
        open_voice_gate(&state);
        let render_state = state.clone();
        let mount = mount_to(container.clone(), move || {
            let state = render_state.clone();
            provide_context(state.clone());
            let agent_ref = Signal::derive(move || state.active_agent.get());
            let composer = render_state.composer();
            view! {
                <VoiceComposerBar agent_ref=agent_ref composer=composer.clone() />
                <VoiceComposerButton agent_ref=agent_ref composer=composer />
            }
        });
        next_tick().await;

        state.voice_ui.set(active_voice_state(1));
        next_tick().await;
        assert!(session_bar(&container).is_some());

        let envelope = protocol::Envelope::from_payload(
            StreamPath("/voice/voice-session".to_owned()),
            FrameKind::VoiceError,
            1,
            &protocol::VoiceErrorPayload {
                session_id: Some(protocol::VoiceSessionId("voice-session".to_owned())),
                generation: 1,
                code: protocol::VoiceErrorCode::ProviderUnavailable,
                retryable: true,
                fatal: true,
                detail: None,
            },
        )
        .expect("encode voice_error envelope");
        handle_control(&state, "local", &envelope);
        next_tick().await;

        let bar = session_bar(&container)
            .expect("a voice error must keep the session surface visible, not vanish");
        let text = bar.text_content().unwrap();
        assert!(
            text.contains("Amazon speech"),
            "the error must name what failed, got: {text}"
        );
        assert!(
            text.contains("retry"),
            "a retryable error must invite a retry, got: {text}"
        );
        assert!(
            start_button(&container).is_none(),
            "the start button must not sit beside an undismissed error"
        );

        let dismiss: HtmlElement = bar
            .query_selector("button")
            .unwrap()
            .expect("the error surface must offer a dismiss action")
            .dyn_into()
            .unwrap();
        dismiss.click();
        next_tick().await;
        assert!(session_bar(&container).is_none());
        assert!(
            start_button(&container).is_some(),
            "dismissing the error must restore the start button"
        );

        drop(mount);
        container.remove();
    }

    #[wasm_bindgen_test]
    async fn dictation_only_new_chat_failure_is_visible_and_dismissable() {
        let _stub = install_voice_listener_stub(None, None);
        let container = container();
        let state = AppState::new();
        state.selected_host_id.set(Some("local".to_owned()));
        state.host_settings_by_host.update(|settings| {
            let mut host = settings_model::HostSettings::default();
            host.voice.dictation_enabled = true;
            host.voice.dictation_region = Some("us-west-2".to_owned());
            settings.insert("local".to_owned(), host);
        });
        state.voice_capabilities_by_host.update(|capabilities| {
            capabilities.insert(
                "local".to_owned(),
                protocol::VoiceCapabilitiesPayload::for_connection(false, true, true),
            );
        });
        state.voice_mode_choice.set(protocol::VoiceMode::Dictation);
        let composer = state.composer();
        let other_composer = ComposerHandle::default();
        let render_state = state.clone();
        let render_composer = composer.clone();
        let render_other_composer = other_composer.clone();
        let mount = mount_to(container.clone(), move || {
            provide_context(render_state.clone());
            let agent_ref = Signal::derive(|| None);
            let other_agent_ref = Signal::derive(|| None);
            view! {
                <VoiceComposerBar agent_ref=agent_ref composer=render_composer.clone() />
                <VoiceComposerButton agent_ref=agent_ref composer=render_composer.clone() />
                <div data-testid="other-composer">
                    <VoiceComposerBar
                        agent_ref=other_agent_ref
                        composer=render_other_composer.clone()
                    />
                </div>
            }
        });
        next_tick().await;
        assert!(
            start_button(&container).is_some(),
            "dictation availability must offer speech in a new-chat composer"
        );
        // Only dictation can run here, so the mic must commit to it rather than
        // making the user consult a separate mode control.
        assert_eq!(
            start_button(&container)
                .unwrap()
                .get_attribute("data-voice-mode")
                .as_deref(),
            Some("dictation")
        );
        let toggle: HtmlElement = container
            .query_selector("[data-test='chat-voice-mode-toggle']")
            .unwrap()
            .expect("mode caret must be offered")
            .dyn_into()
            .unwrap();
        toggle.click();
        next_tick().await;
        let blocked = container
            .query_selector("[data-test='chat-voice-mode-conversation']")
            .unwrap()
            .expect("an unavailable mode stays listed rather than vanishing");
        assert!(
            blocked.has_attribute("disabled"),
            "a mode that cannot start must not be selectable"
        );
        // The whole reason for replacing the disabled `<option>`: the menu now
        // says why the mode is out instead of offering a dead entry.
        assert_eq!(
            blocked.get_attribute("title").as_deref(),
            Some("Voice is turned off for this host")
        );
        toggle.click();
        next_tick().await;

        state.voice_ui.set(VoiceUiState::Active {
            generation: 7,
            host_id: "local".to_owned(),
            session_id: protocol::VoiceSessionId("dictation-session".to_owned()),
            request: protocol::VoiceAcceptedRequest::Dictation {
                uplink: protocol::VoiceAudioFormat::opus(48_000),
            },
            dictation: Some(DictationCapture {
                composer_text: composer.text.clone(),
                finalized: String::new(),
                partial: None,
                finishing: false,
            }),
            state: protocol::VoiceSessionState::Listening,
            lanes: VoiceLanes::default(),
            next_output_media_seq: 0,
            dropped_output_packets: 0,
        });
        next_tick().await;
        assert!(session_bar(&container).is_some());

        let envelope = protocol::Envelope::from_payload(
            StreamPath("/voice/dictation-session".to_owned()),
            FrameKind::VoiceError,
            1,
            &protocol::VoiceErrorPayload {
                session_id: Some(protocol::VoiceSessionId("dictation-session".to_owned())),
                generation: 7,
                code: protocol::VoiceErrorCode::PermissionDenied,
                retryable: false,
                fatal: true,
                detail: None,
            },
        )
        .expect("encode dictation voice_error envelope");
        handle_control(&state, "local", &envelope);
        next_tick().await;

        let bar = session_bar(&container)
            .expect("dictation failure must remain visible in its originating composer");
        assert!(
            bar.text_content().unwrap().contains("lacks permission"),
            "the typed provider failure must be visible"
        );
        assert!(
            container
                .query_selector("[data-testid='other-composer'] [data-testid='voice-session-bar']",)
                .unwrap()
                .is_none(),
            "dictation failure must not appear in a different composer"
        );
        assert!(start_button(&container).is_none());

        let dismiss: HtmlElement = bar
            .query_selector("button")
            .unwrap()
            .expect("dictation failure must offer Dismiss")
            .dyn_into()
            .unwrap();
        dismiss.click();
        next_tick().await;
        assert!(session_bar(&container).is_none());
        assert!(
            start_button(&container).is_some(),
            "Dismiss must restore the dictation start affordance"
        );

        drop(mount);
        container.remove();
    }

    /// The voice band separates the session's three voices into lanes — what
    /// the model hears (You), what it says (Nova), and what the agent is
    /// doing — keeps finalized voice turns OUT of the agent chat unless the
    /// user opts in, and treats collapse as a sticky user choice that session
    /// traffic must never undo.
    #[wasm_bindgen_test]
    async fn voice_band_lanes_chat_gating_and_sticky_collapse() {
        let _stub = install_voice_listener_stub(None, None);
        let container = container();
        let state = AppState::new();
        open_voice_gate(&state);
        let render_state = state.clone();
        let mount = mount_to(container.clone(), move || {
            let state = render_state.clone();
            provide_context(state.clone());
            let agent_ref = Signal::derive(move || state.active_agent.get());
            let composer = render_state.composer();
            view! { <VoiceComposerBar agent_ref=agent_ref composer=composer /> }
        });
        next_tick().await;
        state.voice_ui.set(active_voice_state(1));
        next_tick().await;

        let transcript = |speaker, text: &str, is_final| {
            protocol::Envelope::from_payload(
                StreamPath("/voice/voice-session".to_owned()),
                FrameKind::VoiceTranscript,
                1,
                &protocol::VoiceTranscriptPayload {
                    session_id: protocol::VoiceSessionId("voice-session".to_owned()),
                    generation: 1,
                    speaker,
                    text: text.to_owned(),
                    is_final,
                    message_id: None,
                },
            )
            .expect("encode transcript envelope")
        };
        let chat_row_total = || {
            state
                .chat_rows
                .with_untracked(|rows| rows.values().map(Vec::len).sum::<usize>())
        };

        handle_control(
            &state,
            "local",
            &transcript(
                protocol::VoiceTranscriptSpeaker::User,
                "fix the flaky test",
                false,
            ),
        );
        handle_control(
            &state,
            "local",
            &transcript(
                protocol::VoiceTranscriptSpeaker::Assistant,
                "asking the agent now",
                false,
            ),
        );
        handle_control(
            &state,
            "local",
            &transcript(
                protocol::VoiceTranscriptSpeaker::Progress,
                "agent is reading voice.rs",
                false,
            ),
        );
        next_tick().await;

        let lane_text = |id: &str| {
            container
                .query_selector(&format!("[data-testid='{id}']"))
                .unwrap()
                .unwrap_or_else(|| panic!("missing lane {id}"))
                .text_content()
                .unwrap()
        };
        assert!(lane_text("voice-lane-you").contains("fix the flaky test"));
        assert!(lane_text("voice-lane-nova").contains("asking the agent now"));
        assert!(lane_text("voice-lane-agent").contains("agent is reading voice.rs"));

        handle_control(
            &state,
            "local",
            &transcript(protocol::VoiceTranscriptSpeaker::User, "final words", true),
        );
        next_tick().await;
        assert_eq!(
            chat_row_total(),
            0,
            "voice transcripts must stay out of the chat unless the user opts in"
        );

        state.voice_transcript_in_chat.set(true);
        handle_control(
            &state,
            "local",
            &transcript(
                protocol::VoiceTranscriptSpeaker::User,
                "on the record",
                true,
            ),
        );
        next_tick().await;
        assert_eq!(
            chat_row_total(),
            1,
            "opting in must append finalized turns to the chat"
        );

        let collapse: HtmlElement = container
            .query_selector("[data-testid='voice-band-collapse']")
            .unwrap()
            .expect("expanded band offers a collapse control")
            .dyn_into()
            .unwrap();
        collapse.click();
        next_tick().await;
        assert!(
            container
                .query_selector("[data-testid='voice-band-collapsed']")
                .unwrap()
                .is_some(),
            "collapse must switch to the one-line strip"
        );
        handle_control(
            &state,
            "local",
            &transcript(
                protocol::VoiceTranscriptSpeaker::Assistant,
                "still talking",
                false,
            ),
        );
        next_tick().await;
        assert!(
            container
                .query_selector("[data-testid='voice-band-collapsed']")
                .unwrap()
                .is_some(),
            "session traffic must not expand a collapsed band"
        );
        assert!(
            container
                .query_selector("[data-testid='voice-band-expanded']")
                .unwrap()
                .is_none()
        );

        drop(mount);
        container.remove();
    }

    /// The gate walk that used to be asserted on the floating strip. Same
    /// contract — unresolved target, unstarted agent, unsupported build and
    /// fatal agent each withhold the affordance — now asserted on the composer
    /// button that replaced it. The chat is opened before mount because the
    /// rendered control owns that chat's composer; retaining the detached
    /// new-chat composer across a tab replacement is not production behavior.
    #[wasm_bindgen_test]
    async fn composer_button_reacts_from_unresolved_target_to_available_and_fatal() {
        let container = container();
        let state = AppState::new();
        state.host_settings_by_host.update(|settings| {
            let mut host = settings_model::HostSettings::default();
            host.voice.enabled = true;
            settings.insert("local".to_owned(), host);
        });
        state.voice_capabilities_by_host.update(|capabilities| {
            capabilities.insert(
                "local".to_owned(),
                protocol::VoiceCapabilitiesPayload::for_connection(true, false, true),
            );
        });
        state.open_tab(
            TabContent::chat_with_agent(ActiveAgentRef {
                host_id: "local".to_owned(),
                agent_id: AgentId("voice-agent".to_owned()),
            }),
            "Voice agent".to_owned(),
            true,
        );
        let render_state = state.clone();
        let mount = mount_to(container.clone(), move || {
            let state = render_state.clone();
            provide_context(state.clone());
            let agent_ref = Signal::derive(move || state.active_agent.get());
            let composer = render_state.composer();
            view! { <VoiceComposerButton agent_ref=agent_ref composer=composer /> }
        });
        next_tick().await;

        assert!(start_button(&container).is_none());

        state.agents.set(vec![gate_test_agent(false)]);
        next_tick().await;
        assert!(
            start_button(&container).is_none(),
            "an unstarted agent has no voice target"
        );

        state.agents.update(|agents| agents[0].started = true);
        next_tick().await;
        assert!(start_button(&container).is_some());

        state.native_voice_supported.set(false);
        next_tick().await;
        assert!(start_button(&container).is_none());

        state.native_voice_supported.set(true);
        next_tick().await;
        assert!(start_button(&container).is_some());

        state.agents.update(|agents| {
            agents[0].fatal_error = Some("not exposed".to_owned());
        });
        next_tick().await;
        assert!(start_button(&container).is_none());

        drop(mount);
        container.remove();
    }

    /// A composer showing a chat other than the active one must not render
    /// voice controls: voice always acts on the active agent, so a split-pane
    /// button there would start a session against a different chat.
    #[wasm_bindgen_test]
    async fn composer_button_is_withheld_from_non_active_chats() {
        let container = container();
        let state = AppState::new();
        open_voice_gate(&state);
        let render_state = state.clone();
        let mount = mount_to(container.clone(), move || {
            provide_context(render_state.clone());
            let other = Signal::derive(|| {
                Some(ActiveAgentRef {
                    host_id: "local".to_owned(),
                    agent_id: AgentId("other-agent".to_owned()),
                })
            });
            let composer = render_state.composer();
            view! { <VoiceComposerButton agent_ref=other composer=composer /> }
        });
        next_tick().await;
        assert!(start_button(&container).is_none());
        drop(mount);
        container.remove();
    }

    #[wasm_bindgen_test]
    async fn dictation_keeps_draft_editable_until_provider_completion() {
        let _stub = install_voice_listener_stub(None, None);
        let container = container();
        let state = AppState::new();
        open_voice_gate(&state);
        state.host_settings_by_host.update(|settings| {
            settings.get_mut("local").unwrap().voice.dictation_enabled = true;
            settings.get_mut("local").unwrap().voice.dictation_region =
                Some("us-west-2".to_owned());
        });
        state.voice_capabilities_by_host.update(|capabilities| {
            capabilities.insert(
                "local".to_owned(),
                protocol::VoiceCapabilitiesPayload::for_connection(true, true, true),
            );
        });
        state.voice_mode_choice.set(protocol::VoiceMode::Dictation);
        let composer = state.composer();
        composer.text.set("existing draft".to_owned());
        let render_state = state.clone();
        let render_composer = composer.clone();
        let mount = mount_to(container.clone(), move || {
            let state = render_state.clone();
            provide_context(state.clone());
            let agent_ref = Signal::derive(move || state.active_agent.get());
            view! {
                <VoiceComposerBar agent_ref=agent_ref composer=render_composer.clone() />
                <VoiceComposerButton agent_ref=agent_ref composer=render_composer.clone() />
            }
        });
        next_tick().await;
        // The mic announces the mode it will start, so a user knows what the
        // press does without reading a separate control.
        assert_eq!(
            start_button(&container)
                .expect("mic must be offered")
                .get_attribute("aria-label")
                .as_deref(),
            Some("Dictate to composer")
        );
        assert!(
            container
                .query_selector("[data-test='chat-voice-mode-menu']")
                .unwrap()
                .is_none(),
            "the mode menu must stay closed until its caret is used"
        );
        let toggle: HtmlElement = container
            .query_selector("[data-test='chat-voice-mode-toggle']")
            .unwrap()
            .expect("mode caret must be offered")
            .dyn_into()
            .unwrap();
        toggle.click();
        next_tick().await;
        for (selector, label, checked) in [
            ("chat-voice-mode-dictation", "Dictate to composer", "true"),
            ("chat-voice-mode-conversation", "Talk with Nova", "false"),
        ] {
            let item = container
                .query_selector(&format!("[data-test='{selector}']"))
                .unwrap()
                .expect("both speech modes must stay listed");
            assert!(
                item.text_content().unwrap_or_default().contains(label),
                "the menu must name {label}"
            );
            assert_eq!(
                item.get_attribute("aria-checked").as_deref(),
                Some(checked),
                "the tick must mark the mode the mic would start"
            );
        }
        toggle.click();
        next_tick().await;

        state.voice_ui.set(VoiceUiState::Active {
            generation: 9,
            host_id: "local".to_owned(),
            session_id: protocol::VoiceSessionId("dictation-session".to_owned()),
            request: protocol::VoiceAcceptedRequest::Dictation {
                uplink: protocol::VoiceAudioFormat::opus(48_000),
            },
            dictation: Some(DictationCapture {
                composer_text: composer.text.clone(),
                finalized: String::new(),
                partial: None,
                finishing: true,
            }),
            state: protocol::VoiceSessionState::Ending,
            lanes: VoiceLanes::default(),
            next_output_media_seq: 0,
            dropped_output_packets: 0,
        });
        let transcript = |text: &str, is_final| {
            protocol::Envelope::from_payload(
                StreamPath("/voice/dictation-session".to_owned()),
                FrameKind::VoiceTranscript,
                0,
                &protocol::VoiceTranscriptPayload {
                    session_id: protocol::VoiceSessionId("dictation-session".to_owned()),
                    generation: 9,
                    speaker: protocol::VoiceTranscriptSpeaker::User,
                    text: text.to_owned(),
                    is_final,
                    message_id: None,
                },
            )
            .unwrap()
        };
        handle_control(&state, "local", &transcript("first partial", false));
        handle_control(&state, "local", &transcript("replacement partial", false));
        next_tick().await;
        let partial = container
            .query_selector("[data-testid='dictation-partial']")
            .unwrap()
            .unwrap()
            .text_content()
            .unwrap();
        assert_eq!(partial, "replacement partial");
        assert_eq!(composer.text.get_untracked(), "existing draft");

        handle_control(&state, "local", &transcript("provider final", true));
        composer.text.set("concurrent edit".to_owned());
        let completed = protocol::Envelope::from_payload(
            StreamPath("/voice/dictation-session".to_owned()),
            FrameKind::VoiceStop,
            1,
            &protocol::VoiceStopPayload {
                session_id: protocol::VoiceSessionId("dictation-session".to_owned()),
                generation: 9,
                reason: protocol::VoiceStopReason::ProviderCompleted,
                stats: Default::default(),
            },
        )
        .unwrap();
        handle_control(&state, "local", &completed);
        next_tick().await;
        assert_eq!(
            composer.text.get_untracked(),
            "concurrent edit provider final"
        );
        assert!(state.chat_rows.with_untracked(|rows| rows.is_empty()));

        composer.text.set("cancelled draft".to_owned());
        state.voice_ui.set(VoiceUiState::Active {
            generation: 10,
            host_id: "local".to_owned(),
            session_id: protocol::VoiceSessionId("cancel-session".to_owned()),
            request: protocol::VoiceAcceptedRequest::Dictation {
                uplink: protocol::VoiceAudioFormat::opus(48_000),
            },
            dictation: Some(DictationCapture {
                composer_text: composer.text.clone(),
                finalized: "discard me".to_owned(),
                partial: Some("also discard".to_owned()),
                finishing: false,
            }),
            state: protocol::VoiceSessionState::Listening,
            lanes: VoiceLanes::default(),
            next_output_media_seq: 0,
            dropped_output_packets: 0,
        });
        stop(state.clone(), protocol::VoiceStopReason::UserExited);
        next_tick().await;
        assert_eq!(composer.text.get_untracked(), "cancelled draft");

        drop(mount);
        container.remove();
    }
}
