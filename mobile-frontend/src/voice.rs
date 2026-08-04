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
        target: protocol::VoiceTarget,
    },
    Active {
        generation: u64,
        host: LocalHostId,
        target: protocol::VoiceTarget,
        session: protocol::VoiceSessionId,
        phase: protocol::VoiceSessionState,
        transcript: String,
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

fn start(state: AppState) {
    let Some((host, target)) = active_target(&state) else {
        return;
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
        target: target.clone(),
    });
    spawn_local(async move {
        if let Err(error) = media_call("prepare", JsValue::NULL).await {
            let _ = media_call("stop", JsValue::NULL).await;
            state.voice_ui.set(MobileVoiceState::Failed(error));
            return;
        }
        let payload = protocol::VoiceStartPayload {
            generation,
            target,
            formats: vec![protocol::VoiceFormatPair {
                uplink: protocol::VoiceAudioFormat::opus(48_000),
                downlink: protocol::VoiceAudioFormat::opus(24_000),
            }],
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

fn interrupt(state: AppState) {
    let MobileVoiceState::Active {
        generation,
        host,
        session,
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
                let pending = matches!(
                    state.voice_ui.get_untracked(),
                    MobileVoiceState::Starting { generation, host: ref pending_host, ref target }
                        if generation == payload.generation && pending_host == host && target == &payload.target
                );
                let owns_target = active_target(state).is_some_and(|(active_host, target)| {
                    active_host == *host && target == payload.target
                });
                if !pending || !owns_target {
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
                    target: payload.target,
                    session: payload.session_id,
                    phase: protocol::VoiceSessionState::Listening,
                    transcript: String::new(),
                    dropped_output_packets: 0,
                    next_output_media_seq: 0,
                });
                let media_state = state.clone();
                spawn_local(async move {
                    if let Err(error) =
                        media_call("start", JsValue::from_f64(payload.generation as f64)).await
                    {
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
                        target,
                        ..
                    } if generation == payload.generation
                        && active_host == *host
                        && session == payload.session_id =>
                    {
                        Some((target.agent_id, payload.clone()))
                    }
                    _ => None,
                };
                let Some((agent_id, payload)) = identity else {
                    return;
                };
                state.voice_ui.update(|value| {
                    if let MobileVoiceState::Active { transcript, .. } = value {
                        *transcript = payload.text.clone();
                    }
                });
                if payload.is_final {
                    let agent_ref = crate::state::AgentRef {
                        local_host_id: host.clone(),
                        agent_id,
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
                stop(state.clone(), protocol::VoiceStopReason::ProviderFailed);
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
                    stop(state.clone(), protocol::VoiceStopReason::ProviderFailed);
                }
            }
        }
        _ => {}
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
        let start_state = state.clone();
        let stop_state = state.clone();
        let interrupt_state = state.clone();
        match state.voice_ui.get() {
            MobileVoiceState::Idle => view! {
                <button
                    on:click=move |_| start(start_state.clone())
                    aria-label="Start voice"
                >
                    "Voice"
                </button>
            }
            .into_any(),
            MobileVoiceState::Starting { .. } => view! { <span>"Connecting…"</span> }.into_any(),
            MobileVoiceState::Active {
                phase, transcript, ..
            } => view! {
                <span>{format!("{phase:?}")}</span>
                <span>{transcript}</span>
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
            MobileVoiceState::Failed(error) => view! { <span>{error}</span> }.into_any(),
        }
    }
}

#[component]
pub fn MobileVoiceBar() -> impl IntoView {
    let state = expect_context::<AppState>();
    install(state.clone());
    let old = StoredValue::new(None);
    let switch = state.clone();
    Effect::new(move |_| {
        let current = switch.active_agent.get();
        if old.get_value().is_some() && old.get_value() != current {
            stop(switch.clone(), protocol::VoiceStopReason::TargetChanged)
        }
        old.set_value(current);
    });
    let visible_state = state.clone();
    let visible = Memo::new(move |_| {
        active_target(&visible_state).is_some_and(|(host, _)| {
            visible_state
                .host_settings_by_host
                .with(|v| v.get(&host).is_some_and(|s| s.voice.enabled))
                && visible_state
                    .voice_capabilities_by_host
                    .with(|v| v.get(&host).is_some_and(|c| c.nova_available))
        })
    });
    let render_state = StoredValue::new(state);
    view! {
        <Show when=move || visible.get()>
            <aside class="mobile-voice-bar">
                <MobileVoiceControls state=render_state.get_value() />
            </aside>
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

    #[wasm_bindgen_test]
    async fn controls_react_across_repeated_renders() {
        let container = container();
        let state = AppState::new();
        let render_state = state.clone();
        let mount = mount_to(container.clone(), move || {
            view! { <MobileVoiceControls state=render_state.clone() /> }
        });
        next_tick().await;
        assert!(
            container
                .query_selector("button[aria-label='Start voice']")
                .unwrap()
                .is_some()
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
                .query_selector("button[aria-label='Start voice']")
                .unwrap()
                .is_some()
        );
        drop(mount);
        container.remove();
    }
}
