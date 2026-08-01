use leptos::prelude::*;

use crate::state::AppState;
use crate::voice::{VoiceModel, VoicePhase};

#[derive(Debug, PartialEq, Eq)]
struct VoiceCaptionPresentation {
    visible: String,
    speaker: Option<&'static str>,
    is_final: Option<bool>,
    announcement: Option<String>,
}

fn caption_presentation(
    phase: &VoicePhase,
    caption: Option<String>,
    transcript: Option<protocol::VoiceTranscript>,
) -> VoiceCaptionPresentation {
    match transcript {
        Some(transcript) => {
            let speaker = match transcript.speaker {
                protocol::VoiceTranscriptSpeaker::User => "user",
                protocol::VoiceTranscriptSpeaker::Assistant => "assistant",
            };
            VoiceCaptionPresentation {
                announcement: transcript.is_final.then(|| transcript.text.clone()),
                visible: transcript.text,
                speaker: Some(speaker),
                is_final: Some(transcript.is_final),
            }
        }
        None => {
            let visible = caption.unwrap_or_else(|| phase.caption().to_owned());
            VoiceCaptionPresentation {
                announcement: Some(visible.clone()),
                visible,
                speaker: None,
                is_final: None,
            }
        }
    }
}

#[component]
pub fn VoiceBar() -> impl IntoView {
    let state = use_context::<AppState>().expect("AppState context");
    let model = state.voice.model();

    view! {
        {move || match model.get() {
            VoiceModel::Idle => ().into_any(),
            VoiceModel::Failed { message, .. } => {
                let voice = state.voice.clone();
                view! {
                    <aside class="voice-bar voice-bar-error" data-mobile-test="voice-error">
                        <div class="voice-bar-copy">
                            <strong>"Voice unavailable"</strong>
                            <span role="status" aria-live="polite">{message}</span>
                        </div>
                        <button
                            type="button"
                            class="voice-control voice-control-done"
                            data-mobile-test="voice-error-dismiss"
                            on:click=move |_| voice.end("dismissed")
                        >"Dismiss"</button>
                    </aside>
                }.into_any()
            }
            VoiceModel::Live {
                target,
                phase,
                muted,
                processing,
                playback_blocked,
                caption,
                transcript,
                ..
            } => {
                let mute_voice = state.voice.clone();
                let interrupt_voice = state.voice.clone();
                let done_voice = state.voice.clone();
                let playback_voice = state.voice.clone();
                let phase_class = match &phase {
                    crate::voice::VoicePhase::Listening => "listening",
                    crate::voice::VoicePhase::Working(_) => "working",
                    crate::voice::VoicePhase::Speaking => "speaking",
                    _ => "connecting",
                };
                let presentation = caption_presentation(&phase, caption, transcript);
                view! {
                    <aside
                        class=format!("voice-bar voice-bar-{phase_class}")
                        aria-label="Voice session"
                        data-mobile-test="voice-bar"
                    >
                        <div class="voice-orb" aria-hidden="true">
                            <span></span><span></span><span></span>
                        </div>
                        <div class="voice-bar-copy">
                            <strong class="voice-agent">{target.agent_name}</strong>
                            <span
                                class="voice-caption"
                                aria-live="off"
                                data-voice-speaker=presentation.speaker
                                data-voice-final=presentation.is_final.map(|value| value.to_string())
                                data-mobile-test="voice-caption"
                            >
                                {presentation.visible}
                            </span>
                            <span
                                class="visually-hidden"
                                role="status"
                                aria-live="polite"
                                aria-atomic="true"
                                data-mobile-test="voice-announcement"
                            >
                                {presentation.announcement}
                            </span>
                            <span class="voice-aec" data-mobile-test="voice-aec">
                                {processing.short_label()}
                            </span>
                        </div>
                        <div class="voice-controls">
                            <button
                                type="button"
                                class="voice-control"
                                class:voice-control-active=muted
                                aria-pressed=muted.to_string()
                                aria-label=if muted { "Unmute microphone" } else { "Mute microphone" }
                                data-mobile-test="voice-mute"
                                on:click=move |_| mute_voice.toggle_mute()
                            >{if muted { "Unmute" } else { "Mute" }}</button>
                            <button
                                type="button"
                                class="voice-control"
                                aria-label="Interrupt voice response"
                                data-mobile-test="voice-interrupt"
                                on:click=move |_| interrupt_voice.interrupt()
                            >"Interrupt"</button>
                            <button
                                type="button"
                                class="voice-control voice-control-done"
                                aria-label="End voice session"
                                data-mobile-test="voice-done"
                                on:click=move |_| done_voice.end("user-done")
                            >"Done"</button>
                        </div>
                        {playback_blocked.then(|| view! {
                            <button
                                type="button"
                                class="voice-playback"
                                data-mobile-test="voice-playback"
                                on:click=move |_| playback_voice.resume_playback()
                            >"Tap to hear voice"</button>
                        })}
                    </aside>
                }.into_any()
            }
        }}
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod native_tests {
    use super::*;

    #[test]
    fn caption_accessibility_announces_phase_and_final_text_but_not_interim_asr() {
        assert_eq!(
            caption_presentation(&VoicePhase::Listening, None, None),
            VoiceCaptionPresentation {
                visible: "Listening — just speak".to_owned(),
                speaker: None,
                is_final: None,
                announcement: Some("Listening — just speak".to_owned()),
            }
        );

        let interim = caption_presentation(
            &VoicePhase::Listening,
            Some("ignored duplicate caption".to_owned()),
            Some(protocol::VoiceTranscript {
                speaker: protocol::VoiceTranscriptSpeaker::User,
                text: "interim words".to_owned(),
                is_final: false,
            }),
        );
        assert_eq!(interim.visible, "interim words");
        assert_eq!(interim.speaker, Some("user"));
        assert_eq!(interim.is_final, Some(false));
        assert_eq!(interim.announcement, None);

        let final_text = caption_presentation(
            &VoicePhase::Speaking,
            None,
            Some(protocol::VoiceTranscript {
                speaker: protocol::VoiceTranscriptSpeaker::Assistant,
                text: "final words".to_owned(),
                is_final: true,
            }),
        );
        assert_eq!(final_text.visible, "final words");
        assert_eq!(final_text.speaker, Some("assistant"));
        assert_eq!(final_text.is_final, Some(true));
        assert_eq!(final_text.announcement.as_deref(), Some("final words"));
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::voice::{AudioProcessingReport, BrowserAudioSetting, VoiceTarget};
    use wasm_bindgen::JsCast;

    fn make_container() -> web_sys::HtmlElement {
        let document = web_sys::window().unwrap().document().unwrap();
        let container = document.create_element("div").unwrap();
        document.body().unwrap().append_child(&container).unwrap();
        container.dyn_into::<web_sys::HtmlElement>().unwrap()
    }

    #[wasm_bindgen_test::wasm_bindgen_test]
    async fn renders_hands_free_status_and_all_session_controls() {
        let container = make_container();
        let state = AppState::new();
        let target = VoiceTarget {
            local_host_id: crate::state::LocalHostId("host".to_owned()),
            agent_id: protocol::AgentId("agent".to_owned()),
            instance_stream: protocol::StreamPath("/agent/one".to_owned()),
            agent_name: "Nova helper".to_owned(),
        };
        state.voice.model().set(VoiceModel::Live {
            generation: 7,
            target,
            session: None,
            phase: VoicePhase::Listening,
            muted: false,
            processing: AudioProcessingReport {
                echo_cancellation: BrowserAudioSetting::Enabled,
                ..Default::default()
            },
            playback_blocked: false,
            caption: None,
            transcript: None,
        });
        let model = state.voice.model();
        let mount = leptos::mount::mount_to(container.clone(), move || {
            provide_context(state.clone());
            view! { <VoiceBar /> }
        });

        assert_eq!(
            container
                .query_selector("[data-mobile-test='voice-caption']")
                .unwrap()
                .unwrap()
                .text_content()
                .unwrap(),
            "Listening — just speak"
        );
        for control in ["voice-mute", "voice-interrupt", "voice-done"] {
            assert!(
                container
                    .query_selector(&format!("[data-mobile-test='{control}']"))
                    .unwrap()
                    .is_some()
            );
        }
        let bar = container
            .query_selector("[data-mobile-test='voice-bar']")
            .unwrap()
            .unwrap();
        assert!(bar.get_attribute("role").is_none());
        assert_eq!(
            container
                .query_selector("[data-mobile-test='voice-announcement']")
                .unwrap()
                .unwrap()
                .get_attribute("role")
                .as_deref(),
            Some("status")
        );
        assert!(
            bar.query_selector("[role='status'] button")
                .unwrap()
                .is_none()
        );

        model.update(|model| {
            if let VoiceModel::Live { transcript, .. } = model {
                *transcript = Some(protocol::VoiceTranscript {
                    speaker: protocol::VoiceTranscriptSpeaker::User,
                    text: "interim words".to_owned(),
                    is_final: false,
                });
            }
        });
        next_task().await;
        let caption = container
            .query_selector("[data-mobile-test='voice-caption']")
            .unwrap()
            .unwrap();
        assert_eq!(caption.text_content().as_deref(), Some("interim words"));
        assert!(caption.get_attribute("role").is_none());
        assert_eq!(caption.get_attribute("aria-live").as_deref(), Some("off"));
        assert_eq!(
            container
                .query_selector("[data-mobile-test='voice-announcement']")
                .unwrap()
                .unwrap()
                .text_content()
                .as_deref(),
            Some("")
        );

        model.update(|model| {
            if let VoiceModel::Live {
                transcript: Some(transcript),
                ..
            } = model
            {
                transcript.text = "final words".to_owned();
                transcript.is_final = true;
            }
        });
        next_task().await;
        let caption = container
            .query_selector("[data-mobile-test='voice-caption']")
            .unwrap()
            .unwrap();
        assert_eq!(caption.text_content().as_deref(), Some("final words"));
        assert_eq!(
            container
                .query_selector("[data-mobile-test='voice-announcement']")
                .unwrap()
                .unwrap()
                .text_content()
                .as_deref(),
            Some("final words")
        );

        model.update(|model| {
            if let VoiceModel::Live {
                playback_blocked, ..
            } = model
            {
                *playback_blocked = true;
            }
        });
        next_task().await;
        assert!(
            container
                .query_selector("[data-mobile-test='voice-playback']")
                .unwrap()
                .is_some()
        );
        drop(mount);
        container.remove();
    }

    async fn next_task() {
        let promise = js_sys::Promise::new(&mut |resolve, _reject| {
            web_sys::window()
                .unwrap()
                .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 0)
                .unwrap();
        });
        let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
    }
}
