use std::cell::RefCell;

use leptos::prelude::*;
use wasm_bindgen::{JsCast, JsValue, closure::Closure};
use wasm_bindgen_futures::{JsFuture, spawn_local};

use crate::state::{AppState, LocalHostId};
use protocol::{Envelope, FrameKind, ProtocolFrame, StreamPath};

#[derive(Clone, Debug, PartialEq)]
pub enum MobileVoiceState {
    Idle,
    Starting {
        generation: u64,
        host: LocalHostId,
        request: protocol::VoiceRequest,
    },
    Active {
        generation: u64,
        host: LocalHostId,
        request: protocol::VoiceAcceptedRequest,
        session: protocol::VoiceSessionId,
        phase: protocol::VoiceSessionState,
        partial: String,
        finalized: String,
        finishing: bool,
        dropped_output_packets: u64,
        next_output_media_seq: u64,
    },
    Failed(String),
}

thread_local! { static STATE:RefCell<Option<AppState>>=const{RefCell::new(None)}; }

async fn send<T: serde::Serialize>(
    host: &LocalHostId,
    stream: StreamPath,
    kind: FrameKind,
    payload: &T,
    binary: Vec<u8>,
) -> Result<(), String> {
    let envelope =
        Envelope::from_payload(stream.clone(), kind, 0, payload).map_err(|e| e.to_string())?;
    if !envelope.stream.0.starts_with("/voice") {
        return Err("mobile voice frame has a non-voice route".into());
    }
    if binary.is_empty() {
        if !matches!(
            kind,
            FrameKind::VoiceStart
                | FrameKind::VoiceInputEnd
                | FrameKind::VoiceInterrupt
                | FrameKind::VoiceStop
        ) {
            return Err("mobile binary bridge rejected an unrelated control".into());
        }
    } else {
        if kind != FrameKind::VoiceAudio {
            return Err("mobile binary body requires VoiceAudio".into());
        }
        let audio: protocol::VoiceAudioPayload = envelope
            .parse_payload()
            .map_err(|_| "invalid mobile VoiceAudio")?;
        audio.validate_body(binary.len()).map_err(str::to_owned)?;
        if audio.direction != protocol::VoiceDirection::Input
            || envelope.stream != StreamPath(format!("/voice/{}", audio.session_id.0))
        {
            return Err("mobile voice audio session routing mismatch".into());
        }
    }
    if binary.is_empty() {
        crate::send::send_frame(host, stream, kind, payload)
            .await
            .map(|_| ())
            .map_err(|error| error.to_string())
    } else {
        crate::send::send_binary_frame(host, stream, kind, payload, binary)
            .await
            .map_err(|error| error.to_string())
    }
}

/// Load `voice-media.js` (which installs `window.TydeVoiceMedia`) on first
/// use. The production loader page injects only the bundle's stylesheets and
/// entry script — the `<script src="voice-media.js">` tag in the bundle's own
/// index.html never runs there — so the app must load it itself, resolved
/// against the bundle's versioned asset directory (taken from the injected
/// stylesheet link, the one asset guaranteed to be in the DOM in every
/// environment).
async fn ensure_media_installed() -> Result<(), String> {
    let window = web_sys::window().ok_or("window unavailable")?;
    let installed = js_sys::Reflect::get(&window, &JsValue::from_str("TydeVoiceMedia"))
        .is_ok_and(|value| !value.is_undefined() && !value.is_null());
    if installed {
        return Ok(());
    }
    let document = window.document().ok_or("document unavailable")?;
    let stylesheet_href = document
        .query_selector("link[rel='stylesheet'][href*='styles-']")
        .ok()
        .flatten()
        .and_then(|link| link.get_attribute("href"))
        .ok_or_else(|| {
            "cannot locate the bundle stylesheet to resolve voice-media.js".to_owned()
        })?;
    let base = &stylesheet_href[..stylesheet_href.rfind('/').map_or(0, |index| index + 1)];
    let url = format!("{base}voice-media.js");

    let script = document
        .create_element("script")
        .map_err(js_error)?
        .dyn_into::<web_sys::HtmlScriptElement>()
        .map_err(|_| "script element construction failed".to_owned())?;
    script.set_src(&url);
    let loaded = js_sys::Promise::new(&mut |resolve, reject| {
        script.set_onload(Some(&resolve));
        script.set_onerror(Some(&reject));
    });
    document
        .head()
        .ok_or("document head unavailable")?
        .append_child(&script)
        .map_err(js_error)?;
    JsFuture::from(loaded)
        .await
        .map_err(|_| format!("voice media script failed to load from {url}"))?;

    let installed_now = js_sys::Reflect::get(&window, &JsValue::from_str("TydeVoiceMedia"))
        .is_ok_and(|value| !value.is_undefined() && !value.is_null());
    if installed_now {
        Ok(())
    } else {
        Err(format!(
            "voice media script loaded from {url} but did not install TydeVoiceMedia"
        ))
    }
}

fn media_call(
    name: &str,
    arg: JsValue,
) -> impl std::future::Future<Output = Result<JsValue, String>> {
    let name = name.to_owned();
    async move {
        let window = web_sys::window().ok_or("window unavailable")?;
        let media = js_sys::Reflect::get(&window, &JsValue::from_str("TydeVoiceMedia"))
            .map_err(js_error)?;
        let function = js_sys::Reflect::get(&media, &JsValue::from_str(&name))
            .map_err(js_error)?
            .dyn_into::<js_sys::Function>()
            .map_err(|_| "voice media function unavailable".to_owned())?;
        let value = function.call1(&media, &arg).map_err(js_error)?;
        if let Ok(promise) = value.clone().dyn_into::<js_sys::Promise>() {
            JsFuture::from(promise).await.map_err(js_error)
        } else {
            Ok(value)
        }
    }
}

fn js_error(value: JsValue) -> String {
    value
        .as_string()
        .unwrap_or_else(|| "browser voice media failed".into())
}

fn active_target(state: &AppState) -> Option<(LocalHostId, protocol::VoiceTarget)> {
    let active = state.active_agent.get_untracked()?;
    let agent = state.agents.with_untracked(|agents| {
        agents
            .iter()
            .find(|v| v.local_host_id == active.local_host_id && v.agent_id == active.agent_id)
            .cloned()
    })?;
    Some((
        active.local_host_id,
        protocol::VoiceTarget {
            agent_id: agent.agent_id,
            instance_stream: agent.instance_stream,
        },
    ))
}

/// Dictation is the default for the same reason as on desktop: it runs without
/// an agent or a resolvable target, and it only fills the composer rather than
/// acting on the user's behalf.
pub fn initial_voice_mode() -> protocol::VoiceMode {
    if let Some(storage) =
        web_sys::window().and_then(|window| window.local_storage().ok().flatten())
        && storage
            .get_item("tyde.voice.mode")
            .ok()
            .flatten()
            .as_deref()
            == Some("conversation")
    {
        protocol::VoiceMode::Conversation
    } else {
        protocol::VoiceMode::Dictation
    }
}

fn voice_mode_value(mode: protocol::VoiceMode) -> &'static str {
    match mode {
        protocol::VoiceMode::Conversation => "conversation",
        protocol::VoiceMode::Dictation => "dictation",
    }
}

fn voice_mode_label(mode: protocol::VoiceMode) -> &'static str {
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

/// A distinct glyph per mode so the mic reads as its current mode at a glance:
/// a headset for the conversation, a mic for dictation.
fn voice_mode_icon(mode: protocol::VoiceMode) -> AnyView {
    match mode {
        protocol::VoiceMode::Dictation => view! {
            <svg
                class="voice-mic-icon"
                viewBox="0 0 24 24"
                width="18"
                height="18"
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
                width="18"
                height="18"
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

/// The mode the start button will actually use: the remembered choice when it
/// can run, otherwise whichever mode can.
fn effective_mode(
    conversation: bool,
    dictation: bool,
    chosen: protocol::VoiceMode,
) -> Option<protocol::VoiceMode> {
    let allows = |mode| match mode {
        protocol::VoiceMode::Conversation => conversation,
        protocol::VoiceMode::Dictation => dictation,
    };
    if allows(chosen) {
        return Some(chosen);
    }
    [
        protocol::VoiceMode::Dictation,
        protocol::VoiceMode::Conversation,
    ]
    .into_iter()
    .find(|mode| allows(*mode))
}

fn remember_voice_mode(mode: protocol::VoiceMode) {
    if let Some(storage) =
        web_sys::window().and_then(|window| window.local_storage().ok().flatten())
    {
        let _ = storage.set_item("tyde.voice.mode", voice_mode_value(mode));
    }
}

fn mode_availability(state: &AppState) -> (Option<LocalHostId>, bool, bool) {
    let host = state.active_local_host_id.get_untracked();
    let Some(host) = host else {
        return (None, false, false);
    };
    let settings = state
        .host_settings_by_host
        .with_untracked(|values| values.get(&host).cloned());
    let capabilities = state
        .voice_capabilities_by_host
        .with_untracked(|values| values.get(&host).cloned());
    let conversation = active_target(state).is_some()
        && settings.as_ref().is_some_and(|value| value.voice.enabled)
        && capabilities
            .as_ref()
            .is_some_and(|value| value.nova_available);
    let dictation = settings
        .as_ref()
        .is_some_and(|value| value.voice.dictation_enabled)
        && capabilities
            .as_ref()
            .is_some_and(|value| value.dictation_available);
    (Some(host), conversation, dictation)
}

fn start(state: AppState) {
    let (host, conversation, dictation) = mode_availability(&state);
    let Some(host) = host else {
        return;
    };
    let Some(mode) = effective_mode(
        conversation,
        dictation,
        state.voice_mode_choice.get_untracked(),
    ) else {
        return;
    };
    let request = match mode {
        protocol::VoiceMode::Dictation => protocol::VoiceRequest::Dictation {
            formats: vec![protocol::VoiceAudioFormat::opus(48_000)],
        },
        protocol::VoiceMode::Conversation => {
            let Some((_, target)) = active_target(&state) else {
                return;
            };
            protocol::VoiceRequest::Conversation {
                target,
                formats: vec![protocol::VoiceFormatPair {
                    uplink: protocol::VoiceAudioFormat::opus(48_000),
                    downlink: protocol::VoiceAudioFormat::opus(24_000),
                }],
            }
        }
    };
    let generation = state
        .voice_generation
        .try_update(|value| {
            *value = value.saturating_add(1);
            *value
        })
        .unwrap_or(1);
    state.voice_ui.set(MobileVoiceState::Starting {
        generation,
        host: host.clone(),
        request: request.clone(),
    });
    spawn_local(async move {
        if let Err(error) = ensure_media_installed().await {
            state.voice_ui.set(MobileVoiceState::Failed(error));
            return;
        }
        if let Err(error) = media_call("prepare", JsValue::NULL).await {
            let _ = media_call("stop", JsValue::NULL).await;
            state.voice_ui.set(MobileVoiceState::Failed(error));
            return;
        }
        let payload = protocol::VoiceStartPayload {
            generation,
            request,
        };
        if let Err(error) = send(
            &host,
            StreamPath("/voice".into()),
            FrameKind::VoiceStart,
            &payload,
            vec![],
        )
        .await
        {
            let _ = media_call("stop", JsValue::NULL).await;
            state.voice_ui.set(MobileVoiceState::Failed(error));
        }
    })
}

fn stop(state: AppState, reason: protocol::VoiceStopReason) {
    let current = state.voice_ui.get_untracked();
    state.voice_ui.set(MobileVoiceState::Idle);
    spawn_local(async move {
        let _ = media_call("stop", JsValue::NULL).await;
        if let MobileVoiceState::Active {
            generation,
            host,
            session,
            dropped_output_packets,
            ..
        } = current
        {
            let stats = protocol::VoiceFlowStats {
                dropped_packets: dropped_output_packets,
                ..Default::default()
            };
            let payload = protocol::VoiceStopPayload {
                session_id: session.clone(),
                generation,
                reason,
                stats,
            };
            let _ = send(
                &host,
                StreamPath(format!("/voice/{}", session.0)),
                FrameKind::VoiceStop,
                &payload,
                vec![],
            )
            .await;
        }
    })
}

fn finish_dictation(state: AppState) {
    let MobileVoiceState::Active {
        generation,
        host,
        session,
        request: protocol::VoiceAcceptedRequest::Dictation { .. },
        ..
    } = state.voice_ui.get_untracked()
    else {
        return;
    };
    state.voice_ui.update(|current| {
        if let MobileVoiceState::Active {
            phase,
            partial,
            finishing,
            ..
        } = current
        {
            *phase = protocol::VoiceSessionState::Ending;
            partial.clear();
            *finishing = true;
        }
    });
    spawn_local(async move {
        let _ = media_call("stop", JsValue::NULL).await;
        let payload = protocol::VoiceSessionPayload {
            session_id: session.clone(),
            generation,
        };
        if let Err(error) = send(
            &host,
            StreamPath(format!("/voice/{}", session.0)),
            FrameKind::VoiceInputEnd,
            &payload,
            Vec::new(),
        )
        .await
        {
            state.voice_ui.set(MobileVoiceState::Failed(error));
        }
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

fn interrupt(state: AppState) {
    let MobileVoiceState::Active {
        generation,
        host,
        session,
        request: protocol::VoiceAcceptedRequest::Conversation { .. },
        ..
    } = state.voice_ui.get_untracked()
    else {
        return;
    };
    spawn_local(async move {
        let _ = media_call("flush", JsValue::NULL).await;
        let payload = protocol::VoiceSessionPayload {
            session_id: session.clone(),
            generation,
        };
        let _ = send(
            &host,
            StreamPath(format!("/voice/{}", session.0)),
            FrameKind::VoiceInterrupt,
            &payload,
            Vec::new(),
        )
        .await;
    });
}

pub fn transport_lost(state: &AppState, host: &LocalHostId) {
    if matches!(state.voice_ui.get_untracked(), MobileVoiceState::Starting { host: ref active, .. } | MobileVoiceState::Active { host: ref active, .. } if active == host)
    {
        state.voice_ui.set(MobileVoiceState::Idle);
        spawn_local(async {
            let _ = media_call("stop", JsValue::NULL).await;
        });
    }
}

pub fn handle_control(state: &AppState, host: &LocalHostId, envelope: &Envelope) {
    match envelope.kind {
        FrameKind::VoiceAccepted => {
            if let Ok(payload) = envelope.parse_payload::<protocol::VoiceAcceptedPayload>() {
                let pending = match state.voice_ui.get_untracked() {
                    MobileVoiceState::Starting {
                        generation,
                        host: pending_host,
                        request,
                    } if generation == payload.generation && pending_host == *host => {
                        match (&request, &payload.request) {
                            (
                                protocol::VoiceRequest::Conversation { target, .. },
                                protocol::VoiceAcceptedRequest::Conversation {
                                    target: accepted,
                                    ..
                                },
                            ) => {
                                target == accepted
                                    && active_target(state).is_some_and(
                                        |(active_host, active_target)| {
                                            active_host == *host && active_target == *target
                                        },
                                    )
                            }
                            (
                                protocol::VoiceRequest::Dictation { .. },
                                protocol::VoiceAcceptedRequest::Dictation { .. },
                            ) => true,
                            _ => false,
                        }
                    }
                    _ => false,
                };
                if !pending {
                    let host = host.clone();
                    spawn_local(async move {
                        let stop = protocol::VoiceStopPayload {
                            session_id: payload.session_id.clone(),
                            generation: payload.generation,
                            reason: protocol::VoiceStopReason::TargetChanged,
                            stats: Default::default(),
                        };
                        let _ = send(
                            &host,
                            StreamPath(format!("/voice/{}", payload.session_id.0)),
                            FrameKind::VoiceStop,
                            &stop,
                            Vec::new(),
                        )
                        .await;
                    });
                    return;
                }
                state.voice_ui.set(MobileVoiceState::Active {
                    generation: payload.generation,
                    host: host.clone(),
                    request: payload.request.clone(),
                    session: payload.session_id,
                    phase: protocol::VoiceSessionState::Listening,
                    partial: String::new(),
                    finalized: String::new(),
                    finishing: false,
                    dropped_output_packets: 0,
                    next_output_media_seq: 0,
                });
                let media_state = state.clone();
                spawn_local(async move {
                    let options = js_sys::Object::new();
                    let _ = js_sys::Reflect::set(
                        &options,
                        &"generation".into(),
                        &JsValue::from_f64(payload.generation as f64),
                    );
                    let _ = js_sys::Reflect::set(
                        &options,
                        &"inputOnly".into(),
                        &JsValue::from_bool(matches!(
                            payload.request,
                            protocol::VoiceAcceptedRequest::Dictation { .. }
                        )),
                    );
                    if let Err(error) = media_call("start", options.into()).await {
                        stop(media_state.clone(), protocol::VoiceStopReason::MediaFailed);
                        media_state.voice_ui.set(MobileVoiceState::Failed(error));
                    }
                });
            }
        }
        FrameKind::VoiceTranscript => {
            if let Ok(payload) = envelope.parse_payload::<protocol::VoiceTranscriptPayload>() {
                let identity = match state.voice_ui.get_untracked() {
                    MobileVoiceState::Active {
                        generation,
                        host: active_host,
                        session,
                        request,
                        ..
                    } if generation == payload.generation
                        && active_host == *host
                        && session == payload.session_id =>
                    {
                        Some((request, payload.clone()))
                    }
                    _ => None,
                };
                let Some((request, payload)) = identity else {
                    return;
                };
                if matches!(&request, protocol::VoiceAcceptedRequest::Dictation { .. }) {
                    if payload.speaker != protocol::VoiceTranscriptSpeaker::User
                        || payload.message_id.is_some()
                    {
                        return;
                    }
                    state.voice_ui.update(|value| {
                        if let MobileVoiceState::Active {
                            partial, finalized, ..
                        } = value
                        {
                            if payload.is_final {
                                append_provider_text(finalized, &payload.text);
                                partial.clear();
                            } else {
                                *partial = payload.text.clone();
                            }
                        }
                    });
                    return;
                }
                state.voice_ui.update(|value| {
                    if let MobileVoiceState::Active { partial, .. } = value {
                        *partial = payload.text.clone();
                    }
                });
                if payload.is_final {
                    let protocol::VoiceAcceptedRequest::Conversation { target, .. } = request
                    else {
                        return;
                    };
                    let agent_ref = crate::state::AgentRef {
                        local_host_id: host.clone(),
                        agent_id: target.agent_id,
                    };
                    state.push_chat_message_entry(
                        &agent_ref,
                        crate::state::ChatMessageEntry {
                            message: protocol::ChatMessage {
                                message_id: payload.message_id,
                                timestamp: js_sys::Date::now() as u64,
                                sender: match payload.speaker {
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
                                content: payload.text,
                                reasoning: None,
                                tool_calls: Vec::new(),
                                model_info: None,
                                token_usage: None,
                                context_breakdown: None,
                                images: None,
                            },
                            tool_requests: Vec::new(),
                        },
                    );
                }
            }
        }
        FrameKind::VoiceState => {
            if let Ok(payload) = envelope.parse_payload::<protocol::VoiceStatePayload>() {
                let mut flush = false;
                state.voice_ui.update(|value| {
                    if let MobileVoiceState::Active {
                        generation,
                        host: active_host,
                        session,
                        phase,
                        ..
                    } = value
                        && *generation == payload.generation
                        && *active_host == *host
                        && *session == payload.session_id
                    {
                        *phase = payload.state;
                        flush = payload.state == protocol::VoiceSessionState::Interrupting;
                    }
                });
                if flush {
                    spawn_local(async {
                        let _ = media_call("flush", JsValue::NULL).await;
                    });
                }
            }
        }
        FrameKind::VoiceStop => {
            if let Ok(payload) = envelope.parse_payload::<protocol::VoiceStopPayload>()
                && matches!(state.voice_ui.get_untracked(), MobileVoiceState::Active { generation, ref session, .. } if generation == payload.generation && session == &payload.session_id)
            {
                if payload.reason == protocol::VoiceStopReason::ProviderCompleted
                    && let MobileVoiceState::Active {
                        request: protocol::VoiceAcceptedRequest::Dictation { .. },
                        finalized,
                        finishing: true,
                        ..
                    } = state.voice_ui.get_untracked()
                {
                    state
                        .chat_input
                        .update(|draft| append_provider_text(draft, &finalized));
                }
                state.voice_ui.set(MobileVoiceState::Idle);
                spawn_local(async {
                    let _ = media_call("stop", JsValue::NULL).await;
                });
            }
        }
        FrameKind::VoiceError => {
            if let Ok(payload) = envelope.parse_payload::<protocol::VoiceErrorPayload>() {
                let applies = match state.voice_ui.get_untracked() {
                    MobileVoiceState::Starting { generation, .. } => {
                        payload.session_id.is_none() && generation == payload.generation
                    }
                    MobileVoiceState::Active {
                        generation,
                        ref session,
                        ..
                    } => {
                        generation == payload.generation
                            && payload.session_id.as_ref() == Some(session)
                    }
                    _ => false,
                };
                if applies {
                    let message = mobile_voice_error_message(
                        payload.code,
                        payload.detail.as_deref().unwrap_or_default(),
                    );
                    stop(state.clone(), protocol::VoiceStopReason::ProviderFailed);
                    state.voice_ui.set(MobileVoiceState::Failed(message));
                }
            }
        }
        _ => {}
    }
}

fn mobile_voice_error_message(code: protocol::VoiceErrorCode, detail: &str) -> String {
    let prefix = match code {
        protocol::VoiceErrorCode::MissingCredentials => "AWS credentials are unavailable",
        protocol::VoiceErrorCode::PermissionDenied => "AWS permission was denied",
        protocol::VoiceErrorCode::QuotaExceeded => "AWS Transcribe quota was exceeded",
        protocol::VoiceErrorCode::InvalidConfiguration => "Voice configuration is invalid",
        protocol::VoiceErrorCode::ProviderUnavailable => "The voice provider is unavailable",
        _ => "Voice failed",
    };
    if detail.is_empty() {
        prefix.to_owned()
    } else {
        format!("{prefix}: {detail}")
    }
}

pub fn handle_binary_frame(frame: ProtocolFrame) -> Result<(), String> {
    STATE.with(|slot| {
        let Some(state) = slot.borrow().clone() else {
            return Err("mobile voice state is unavailable".into());
        };
        if frame.envelope.kind != FrameKind::VoiceAudio {
            return Err("unauthorized mobile binary frame".into());
        }
        let payload = frame
            .envelope
            .parse_payload::<protocol::VoiceAudioPayload>()
            .map_err(|_| "invalid mobile output VoiceAudio")?;
        payload
            .validate_body(frame.binary.len())
            .map_err(str::to_owned)?;
        if payload.direction != protocol::VoiceDirection::Output
            || frame.envelope.stream != StreamPath(format!("/voice/{}", payload.session_id.0))
        {
            return Err("mobile output voice route mismatch".into());
        }
        let mut admitted = false;
        state.voice_ui.update(|current| {
            if let MobileVoiceState::Active {
                generation,
                session,
                request: protocol::VoiceAcceptedRequest::Conversation { .. },
                next_output_media_seq,
                dropped_output_packets,
                ..
            } = current
                && *generation == payload.generation
                && session.0 == payload.session_id.0
                && payload.first_media_seq >= *next_output_media_seq
            {
                *dropped_output_packets = dropped_output_packets.saturating_add(
                    payload
                        .first_media_seq
                        .saturating_sub(*next_output_media_seq),
                );
                *next_output_media_seq =
                    payload.first_media_seq + payload.packet_lengths.len() as u64;
                admitted = true;
            }
        });
        if !admitted {
            return Ok(());
        }
        let array = js_sys::Uint8Array::from(frame.binary.as_slice());
        let object = js_sys::Object::new();
        let _ = js_sys::Reflect::set(
            &object,
            &"generation".into(),
            &JsValue::from_f64(payload.generation as f64),
        );
        let _ = js_sys::Reflect::set(
            &object,
            &"timestamp".into(),
            &JsValue::from_f64(payload.timestamp_samples_48k as f64 * 1_000_000.0 / 48_000.0),
        );
        let _ = js_sys::Reflect::set(&object, &"opus".into(), &array);
        spawn_local(async move {
            let _ = media_call("push", object.into()).await;
        });
        Ok(())
    })
}

fn install(state: AppState) {
    STATE.with(|slot| *slot.borrow_mut() = Some(state.clone()));
    let Some(window) = web_sys::window() else {
        return;
    };
    let packet_state = state.clone();
    let packet =
        Closure::<dyn FnMut(web_sys::CustomEvent)>::new(move |event: web_sys::CustomEvent| {
            let detail = event.detail();
            let generation = js_sys::Reflect::get(&detail, &"generation".into())
                .ok()
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0) as u64;
            let seq = js_sys::Reflect::get(&detail, &"seq".into())
                .ok()
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0) as u64;
            let data = js_sys::Uint8Array::new(
                &js_sys::Reflect::get(&detail, &"opus".into()).unwrap_or(JsValue::UNDEFINED),
            )
            .to_vec();
            let MobileVoiceState::Active {
                host,
                session,
                generation: current,
                ..
            } = packet_state.voice_ui.get_untracked()
            else {
                return;
            };
            if generation != current {
                return;
            }
            let payload = protocol::VoiceAudioPayload {
                session_id: session.clone(),
                generation,
                direction: protocol::VoiceDirection::Input,
                first_media_seq: seq,
                timestamp_samples_48k: seq * 960,
                packet_lengths: vec![data.len() as u16],
            };
            spawn_local(async move {
                let _ = send(
                    &host,
                    StreamPath(format!("/voice/{}", session.0)),
                    FrameKind::VoiceAudio,
                    &payload,
                    data,
                )
                .await;
            });
        });
    let _ = window.add_event_listener_with_callback(
        "tyde-mobile-voice-packet",
        packet.as_ref().unchecked_ref(),
    );
    packet.forget();
    let lifecycle_state = state.clone();
    let lifecycle = Closure::<dyn FnMut()>::new(move || {
        stop(
            lifecycle_state.clone(),
            protocol::VoiceStopReason::ClientBackgrounded,
        )
    });
    let _ = window.add_event_listener_with_callback(
        "tyde-mobile-voice-lifecycle",
        lifecycle.as_ref().unchecked_ref(),
    );
    lifecycle.forget();
    let drop_state = state.clone();
    let drop_event =
        Closure::<dyn FnMut(web_sys::CustomEvent)>::new(move |event: web_sys::CustomEvent| {
            let detail = event.detail();
            let generation = js_sys::Reflect::get(&detail, &"generation".into())
                .ok()
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0) as u64;
            let packets = js_sys::Reflect::get(&detail, &"packets".into())
                .ok()
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0) as u64;
            drop_state.voice_ui.update(|current| {
                if let MobileVoiceState::Active {
                    generation: active,
                    dropped_output_packets,
                    ..
                } = current
                    && *active == generation
                {
                    *dropped_output_packets = dropped_output_packets.saturating_add(packets);
                }
            });
        });
    let _ = window.add_event_listener_with_callback(
        "tyde-mobile-voice-playback-drop",
        drop_event.as_ref().unchecked_ref(),
    );
    drop_event.forget();
    let error_state = state;
    let error =
        Closure::<dyn FnMut(web_sys::CustomEvent)>::new(move |event: web_sys::CustomEvent| {
            let message = js_sys::Reflect::get(&event.detail(), &"message".into())
                .ok()
                .and_then(|v| v.as_string())
                .unwrap_or_else(|| "Browser Opus or microphone unavailable".into());
            stop(
                error_state.clone(),
                protocol::VoiceStopReason::ProviderFailed,
            );
            error_state.voice_ui.set(MobileVoiceState::Failed(message));
        });
    let _ = window.add_event_listener_with_callback(
        "tyde-mobile-voice-error",
        error.as_ref().unchecked_ref(),
    );
    error.forget();
}

#[component]
fn MobileVoiceControls(state: AppState) -> impl IntoView {
    move || {
        let stop_state = state.clone();
        let interrupt_state = state.clone();
        let finish_state = state.clone();
        let cancel_state = state.clone();
        let dismiss_state = state.clone();
        match state.voice_ui.get() {
            // Idle has no session surface: the start control is
            // [`MobileVoiceComposerButton`], beside Send in the composer row.
            MobileVoiceState::Idle => ().into_any(),
            MobileVoiceState::Starting { request, .. } => view! {
                <span>{if request.mode() == protocol::VoiceMode::Dictation {
                    "Connecting dictation…"
                } else {
                    "Connecting to Nova…"
                }}</span>
                <button
                    aria-label="Cancel voice"
                    on:click=move |_| {
                        stop(cancel_state.clone(), protocol::VoiceStopReason::UserExited)
                    }
                >
                    "Cancel"
                </button>
            }
            .into_any(),
            MobileVoiceState::Active {
                request: protocol::VoiceAcceptedRequest::Conversation { .. },
                phase,
                partial,
                ..
            } => view! {
                <span>{format!("{phase:?}")}</span>
                <span>{partial}</span>
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
            MobileVoiceState::Active {
                request: protocol::VoiceAcceptedRequest::Dictation { .. },
                partial,
                finishing,
                ..
            } => view! {
                <span>{if finishing { "Finishing…" } else { "Listening…" }}</span>
                <span aria-live="polite">{partial}</span>
                <button
                    disabled=finishing
                    on:click=move |_| finish_dictation(finish_state.clone())
                >
                    "Finish"
                </button>
                <button on:click=move |_| {
                    stop(cancel_state.clone(), protocol::VoiceStopReason::UserExited)
                }>
                    "Cancel"
                </button>
            }
            .into_any(),
            MobileVoiceState::Failed(error) => view! {
                <span>{error}</span>
                <button
                    aria-label="Dismiss voice error"
                    on:click=move |_| {
                        stop(dismiss_state.clone(), protocol::VoiceStopReason::UserExited)
                    }
                >
                    "Dismiss"
                </button>
            }
            .into_any(),
        }
    }
}

/// Which speech modes the active host can start right now, plus why a mode
/// cannot. Tracked reads only: this drives composer rendering, and a memo
/// that short-circuits through untracked reads before any chat is active
/// computes once with no subscriptions and never reappears.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct VoiceOffer {
    conversation: Option<&'static str>,
    dictation: Option<&'static str>,
}

impl VoiceOffer {
    fn block(self, mode: protocol::VoiceMode) -> Option<&'static str> {
        match mode {
            protocol::VoiceMode::Conversation => self.conversation,
            protocol::VoiceMode::Dictation => self.dictation,
        }
    }

    fn any(self) -> bool {
        self.conversation.is_none() || self.dictation.is_none()
    }

    fn effective(self, chosen: protocol::VoiceMode) -> Option<protocol::VoiceMode> {
        effective_mode(
            self.conversation.is_none(),
            self.dictation.is_none(),
            chosen,
        )
    }
}

fn voice_offer(state: &AppState) -> Memo<VoiceOffer> {
    let state = state.clone();
    Memo::new(move |_| {
        let Some(host) = state.active_local_host_id.get() else {
            return VoiceOffer {
                conversation: Some("Connect to a host to talk with Nova"),
                dictation: Some("Connect to a host to dictate"),
            };
        };
        let has_conversation_target = state.active_agent.get().is_some_and(|active| {
            active.local_host_id == host
                && state.agents.with(|agents| {
                    agents.iter().any(|agent| {
                        agent.local_host_id == active.local_host_id
                            && agent.agent_id == active.agent_id
                    })
                })
        });
        let settings = state
            .host_settings_by_host
            .with(|values| values.get(&host).cloned());
        let capabilities = state
            .voice_capabilities_by_host
            .with(|values| values.get(&host).cloned());
        let conversation = if !settings.as_ref().is_some_and(|value| value.voice.enabled) {
            Some("Nova voice is off in this host's settings")
        } else if !capabilities
            .as_ref()
            .is_some_and(|value| value.nova_available)
        {
            Some("Nova is not available on this host")
        } else if !has_conversation_target {
            Some("Open a chat to talk with Nova")
        } else {
            None
        };
        let dictation = if !settings
            .as_ref()
            .is_some_and(|value| value.voice.dictation_enabled)
        {
            Some("Dictation is off in this host's settings")
        } else if !capabilities
            .as_ref()
            .is_some_and(|value| value.dictation_available)
        {
            Some("Transcribe is not available on this host")
        } else {
            None
        };
        VoiceOffer {
            conversation,
            dictation,
        }
    })
}

/// Root-mounted and renders nothing: it owns the browser media listeners and
/// the stop-on-target-change effect for the whole app. Every visible voice
/// control lives in the composer ([`MobileVoiceComposerButton`],
/// [`MobileVoiceComposerBar`]).
#[component]
pub fn MobileVoiceRuntime() -> impl IntoView {
    let state = expect_context::<AppState>();
    install(state.clone());
    let old = StoredValue::new((None, None));
    let switch = state;
    Effect::new(move |_| {
        let current = (switch.active_local_host_id.get(), switch.active_agent.get());
        let session_active = matches!(
            switch.voice_ui.get_untracked(),
            MobileVoiceState::Starting { .. } | MobileVoiceState::Active { .. }
        );
        if session_active && old.get_value() != current {
            stop(switch.clone(), protocol::VoiceStopReason::TargetChanged)
        }
        old.set_value(current);
    });
}

/// Start-speech affordance, rendered in the composer row beside Send.
///
/// It lives in the row rather than in a floating pill because the pill was
/// `position: fixed` above the bottom nav, exactly where the composer sits, so
/// it covered the last messages and got in the way of the input. It is a split
/// button rather than a mode `<select>` plus a mic: the mode is a sticky
/// preference set once, so it belongs behind the caret with the other composer
/// menus. The caret lists both modes always; a mode that cannot start stays
/// visible but disabled and says why.
#[component]
pub fn MobileVoiceComposerButton() -> impl IntoView {
    let state = expect_context::<AppState>();
    let offer = voice_offer(&state);
    let idle_state = state.clone();
    let show = Memo::new(move |_| {
        offer.get().any() && matches!(idle_state.voice_ui.get(), MobileVoiceState::Idle)
    });
    let mode_state = state.clone();
    let effective = Memo::new(move |_| offer.get().effective(mode_state.voice_mode_choice.get()));
    let menu_open = RwSignal::new(false);
    let choice_state = StoredValue::new(state.clone());
    let start_state = StoredValue::new(state);
    let mode_item = move |mode: protocol::VoiceMode| {
        let block = Memo::new(move |_| offer.get().block(mode));
        let selected = Memo::new(move |_| effective.get() == Some(mode));
        view! {
            <button
                type="button"
                class="chat-send-menu-item"
                role="menuitemradio"
                aria-checked=move || if selected.get() { "true" } else { "false" }
                data-test=format!("mobile-voice-mode-{}", voice_mode_value(mode))
                disabled=move || block.get().is_some()
                on:click=move |_| {
                    choice_state.with_value(|state| state.voice_mode_choice.set(mode));
                    remember_voice_mode(mode);
                    menu_open.set(false);
                }
            >
                <span class="chat-send-menu-label">
                    {voice_mode_label(mode)}
                    <span class="chat-send-menu-hint">
                        {move || block.get().unwrap_or(voice_mode_hint(mode))}
                    </span>
                </span>
                <span class="chat-send-menu-check" aria-hidden="true">
                    {move || if selected.get() { "✓" } else { "" }}
                </span>
            </button>
        }
    };
    view! {
        <Show when=move || show.get()>
            <div
                class="chat-send-split chat-voice-split"
                role="group"
                aria-label="Speech actions"
                data-mobile-test="chat-voice-split"
            >
                <button
                    type="button"
                    class="chat-voice-btn chat-send-split-primary"
                    data-test="mobile-voice-start"
                    data-voice-mode=move || {
                        effective.get().map(voice_mode_value).unwrap_or("none")
                    }
                    aria-label=move || {
                        effective.get().map(voice_mode_label).unwrap_or("Start speech")
                    }
                    disabled=move || effective.get().is_none()
                    on:click=move |_| {
                        let Some(mode) = effective.get_untracked() else {
                            return;
                        };
                        start_state.with_value(|state| {
                            state.voice_mode_choice.set(mode);
                            start(state.clone());
                        });
                    }
                >
                    {move || effective.get().map(voice_mode_icon)}
                </button>
                <button
                    type="button"
                    class="send-menu-toggle"
                    data-test="mobile-voice-mode-toggle"
                    aria-haspopup="menu"
                    aria-expanded=move || if menu_open.get() { "true" } else { "false" }
                    aria-label="Choose speech mode"
                    on:click=move |_| menu_open.update(|open| *open = !*open)
                >
                    <span aria-hidden="true">"\u{2304}"</span>
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
                        data-test="mobile-voice-mode-menu"
                    >
                        {mode_item(protocol::VoiceMode::Dictation)}
                        {mode_item(protocol::VoiceMode::Conversation)}
                    </div>
                </Show>
            </div>
        </Show>
    }
}

/// Live-session strip, rendered in the composer stack above the input row so
/// it pushes the input down instead of covering the conversation. It shows for
/// every non-idle state, including a failure, so the way out (Cancel, Done,
/// Dismiss) is always reachable.
#[component]
pub fn MobileVoiceComposerBar() -> impl IntoView {
    let state = expect_context::<AppState>();
    let mode_state = state.clone();
    let in_voice_mode =
        Memo::new(move |_| !matches!(mode_state.voice_ui.get(), MobileVoiceState::Idle));
    let render_state = StoredValue::new(state);
    view! {
        <Show when=move || in_voice_mode.get()>
            <div class="chat-voice-bar" data-mobile-test="voice-session-bar">
                <MobileVoiceControls state=render_state.get_value() />
            </div>
        </Show>
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use leptos::mount::mount_to;
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

    fn activate_voice_chat(state: &AppState, host: &str, agent: &str, dictation: bool) {
        let host = LocalHostId(host.to_owned());
        let agent_id = protocol::AgentId(agent.to_owned());
        state.voice_capabilities_by_host.update(|caps| {
            caps.insert(
                host.clone(),
                protocol::VoiceCapabilitiesPayload::for_connection(true, dictation, false),
            );
        });
        // Dictation is switched on in settings either way; `dictation` only
        // controls whether Transcribe is actually available, so the block
        // reason under test is the capability one.
        let mut settings = settings_model::HostSettings::default();
        settings.voice.enabled = true;
        settings.voice.dictation_enabled = true;
        state.host_settings_by_host.update(|map| {
            map.insert(host.clone(), settings);
        });
        state.agents.set(vec![crate::state::AgentInfo {
            local_host_id: host.clone(),
            agent_id: agent_id.clone(),
            name: "Voice agent".to_owned(),
            origin: protocol::AgentOrigin::User,
            backend_kind: protocol::BackendKind::Codex,
            workspace_roots: Vec::new(),
            project_id: None,
            parent_agent_id: None,
            session_id: None,
            custom_agent_id: None,
            created_at_ms: 0,
            instance_stream: protocol::StreamPath(format!("/agent/{agent}/instance")),
            started: true,
            fatal_error: None,
        }]);
        state.active_local_host_id.set(Some(host.clone()));
        state.active_agent.set(Some(crate::state::ActiveAgentRef {
            local_host_id: host,
            agent_id,
        }));
    }

    /// **Without a voice-capable host there is no start control at all**, and
    /// the session bar follows the session state across repeated renders: a
    /// failure is shown with its reason and Dismiss returns to idle.
    #[wasm_bindgen_test]
    async fn controls_react_across_repeated_renders() {
        let container = container();
        let state = AppState::new();
        let mount_state = state.clone();
        let mount = mount_to(container.clone(), move || {
            provide_context(mount_state.clone());
            view! {
                <MobileVoiceComposerBar />
                <MobileVoiceComposerButton />
            }
        });
        next_tick().await;
        assert!(
            container
                .query_selector("[data-test='mobile-voice-start']")
                .unwrap()
                .is_none(),
            "no speech control may render without a voice-capable host"
        );

        state
            .voice_ui
            .set(MobileVoiceState::Failed("voice unavailable".into()));
        next_tick().await;
        assert!(
            container
                .text_content()
                .unwrap()
                .contains("voice unavailable")
        );

        state.voice_ui.set(MobileVoiceState::Idle);
        next_tick().await;
        assert!(
            container
                .query_selector("[data-mobile-test='voice-session-bar']")
                .unwrap()
                .is_none(),
            "the session bar must leave with the session"
        );
        assert!(
            container
                .query_selector("[data-test='mobile-voice-start']")
                .unwrap()
                .is_none(),
            "no speech control may render without a voice-capable host"
        );
        drop(mount);
        container.remove();
    }

    /// **Opening a chat must reveal the mic.**
    ///
    /// The composer mounts before any chat is active, so the offer memo starts
    /// with nothing to offer. It used to compute that through untracked reads,
    /// subscribing to nothing — the memo never recomputed and the Voice button
    /// could never appear, even on a voice-capable host. This drives the live
    /// order: capabilities and settings arrive on connect, the user opens a
    /// chat afterwards. The unconfigured mode stays listed behind the caret,
    /// disabled, with the reason.
    #[wasm_bindgen_test]
    async fn voice_bar_appears_when_a_chat_becomes_active_after_mount() {
        let container = container();
        let state = AppState::new();
        let mount_state = state.clone();
        let mount = mount_to(container.clone(), move || {
            provide_context(mount_state.clone());
            view! { <MobileVoiceComposerButton /> }
        });
        next_tick().await;
        assert!(
            container
                .query_selector("[data-test='mobile-voice-start']")
                .unwrap()
                .is_none(),
            "no voice button may show before a chat is active"
        );

        activate_voice_chat(&state, "voice-host", "voice-agent", false);
        next_tick().await;

        let start = container
            .query_selector("[data-test='mobile-voice-start']")
            .unwrap()
            .expect("the mic must appear once a chat is active on a voice-capable host");
        assert!(
            !start.has_attribute("disabled"),
            "the configured mode must be startable"
        );
        assert_eq!(
            start.get_attribute("data-voice-mode").as_deref(),
            Some("conversation"),
            "with Transcribe unconfigured the mic must fall through to Nova"
        );

        container
            .query_selector("[data-test='mobile-voice-mode-toggle']")
            .unwrap()
            .expect("the caret must open the mode menu")
            .dyn_into::<HtmlElement>()
            .unwrap()
            .click();
        next_tick().await;
        let dictation = container
            .query_selector("[data-test='mobile-voice-mode-dictation']")
            .unwrap()
            .expect("an unconfigured mode stays listed");
        assert!(
            dictation.has_attribute("disabled"),
            "dictation must not be selectable without Transcribe"
        );
        assert!(
            dictation
                .text_content()
                .unwrap()
                .contains("Transcribe is not available"),
            "a disabled mode must say why"
        );
        assert!(
            !container
                .query_selector("[data-test='mobile-voice-mode-conversation']")
                .unwrap()
                .expect("the configured mode is listed")
                .has_attribute("disabled"),
            "the configured mode must be selectable"
        );
        drop(mount);
        container.remove();
    }

    #[wasm_bindgen_test]
    async fn dictation_replaces_partials_and_appends_only_after_finish() {
        let container = container();
        let state = AppState::new();
        state.chat_input.set("existing draft".to_owned());
        state.voice_ui.set(MobileVoiceState::Active {
            generation: 7,
            host: LocalHostId("dictation-host".to_owned()),
            request: protocol::VoiceAcceptedRequest::Dictation {
                uplink: protocol::VoiceAudioFormat::opus(48_000),
            },
            session: protocol::VoiceSessionId("dictation-session".to_owned()),
            phase: protocol::VoiceSessionState::Listening,
            partial: String::new(),
            finalized: String::new(),
            finishing: true,
            dropped_output_packets: 0,
            next_output_media_seq: 0,
        });
        let render_state = state.clone();
        let mount = mount_to(container.clone(), move || {
            view! { <MobileVoiceControls state=render_state.clone() /> }
        });
        let host = LocalHostId("dictation-host".to_owned());
        let transcript = |text: &str, is_final| {
            Envelope::from_payload(
                StreamPath("/voice/dictation-session".to_owned()),
                FrameKind::VoiceTranscript,
                0,
                &protocol::VoiceTranscriptPayload {
                    session_id: protocol::VoiceSessionId("dictation-session".to_owned()),
                    generation: 7,
                    speaker: protocol::VoiceTranscriptSpeaker::User,
                    text: text.to_owned(),
                    is_final,
                    message_id: None,
                },
            )
            .unwrap()
        };

        handle_control(&state, &host, &transcript("provisional", false));
        handle_control(&state, &host, &transcript("replacement", false));
        next_tick().await;
        let visible = container.text_content().unwrap();
        assert!(visible.contains("replacement"));
        assert!(!visible.contains("provisional"));
        assert_eq!(state.chat_input.get_untracked(), "existing draft");

        handle_control(&state, &host, &transcript("provider final", true));
        state.chat_input.set("concurrent edit".to_owned());
        let stop_envelope = Envelope::from_payload(
            StreamPath("/voice/dictation-session".to_owned()),
            FrameKind::VoiceStop,
            1,
            &protocol::VoiceStopPayload {
                session_id: protocol::VoiceSessionId("dictation-session".to_owned()),
                generation: 7,
                reason: protocol::VoiceStopReason::ProviderCompleted,
                stats: Default::default(),
            },
        )
        .unwrap();
        handle_control(&state, &host, &stop_envelope);
        next_tick().await;
        assert_eq!(
            state.chat_input.get_untracked(),
            "concurrent edit provider final"
        );
        assert!(
            state
                .chat_messages
                .with_untracked(|messages| messages.is_empty())
        );

        state.chat_input.set("cancelled draft".to_owned());
        state.voice_ui.set(MobileVoiceState::Active {
            generation: 8,
            host,
            request: protocol::VoiceAcceptedRequest::Dictation {
                uplink: protocol::VoiceAudioFormat::opus(48_000),
            },
            session: protocol::VoiceSessionId("cancel-session".to_owned()),
            phase: protocol::VoiceSessionState::Listening,
            partial: "discard me".to_owned(),
            finalized: "also discard".to_owned(),
            finishing: false,
            dropped_output_packets: 0,
            next_output_media_seq: 0,
        });
        stop(state.clone(), protocol::VoiceStopReason::UserExited);
        next_tick().await;
        assert_eq!(state.chat_input.get_untracked(), "cancelled draft");

        drop(mount);
        container.remove();
    }
}
