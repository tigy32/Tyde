use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

use super::model::{AudioProcessingReport, IceCandidate};

pub type VoiceFuture<T> = Pin<Box<dyn Future<Output = T>>>;

#[derive(Clone)]
pub struct MediaCallbacks {
    pub ice_candidate: Rc<dyn Fn(Option<IceCandidate>)>,
    pub connection_failed: Rc<dyn Fn(String)>,
    pub connection_interrupted: Rc<dyn Fn()>,
    pub connection_recovered: Rc<dyn Fn()>,
    pub remote_audio_started: Rc<dyn Fn()>,
    pub playback_blocked: Rc<dyn Fn()>,
}

pub struct PreparedMedia {
    pub offer_sdp: String,
    pub processing: AudioProcessingReport,
    pub session: Rc<dyn VoiceMediaSession>,
}

pub trait VoiceMediaFactory {
    fn prepare(&self, callbacks: MediaCallbacks) -> VoiceFuture<Result<PreparedMedia, String>>;
}

pub trait VoiceMediaSession {
    fn apply_answer(&self, sdp: String) -> VoiceFuture<Result<(), String>>;
    fn add_remote_candidate(&self, candidate: IceCandidate) -> VoiceFuture<Result<(), String>>;
    fn finish_remote_candidates(&self) -> VoiceFuture<Result<(), String>>;
    fn set_muted(&self, muted: bool);
    fn interrupt_playback(&self);
    fn resume_playback(&self) -> VoiceFuture<Result<(), String>>;
    fn close_now(&self);
}

#[cfg(any(test, target_arch = "wasm32"))]
fn is_signal_candidate(candidate: &str) -> bool {
    candidate.starts_with("candidate:")
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Default)]
pub struct BrowserVoiceMedia;

#[cfg(not(target_arch = "wasm32"))]
impl VoiceMediaFactory for BrowserVoiceMedia {
    fn prepare(&self, _callbacks: MediaCallbacks) -> VoiceFuture<Result<PreparedMedia, String>> {
        Box::pin(async { Err("Voice media is available only in the browser app.".to_owned()) })
    }
}

#[cfg(target_arch = "wasm32")]
mod browser {
    use std::cell::RefCell;

    use js_sys::Reflect;
    use wasm_bindgen::{JsCast, JsValue, closure::Closure};
    use wasm_bindgen_futures::JsFuture;
    use web_sys::{
        HtmlAudioElement, MediaStream, MediaStreamConstraints, MediaStreamTrack,
        MediaTrackConstraints, RtcConfiguration, RtcIceCandidateInit, RtcPeerConnection,
        RtcPeerConnectionIceEvent, RtcPeerConnectionState, RtcSdpType, RtcSessionDescriptionInit,
        RtcTrackEvent,
    };

    use super::*;
    use crate::voice::model::BrowserAudioSetting;

    #[derive(Default)]
    pub struct BrowserVoiceMedia;

    struct BrowserVoiceSession {
        peer: RtcPeerConnection,
        local_stream: MediaStream,
        audio: HtmlAudioElement,
        callbacks: RefCell<Option<BrowserCallbacks>>,
    }

    struct BrowserCallbacks {
        on_ice: Closure<dyn FnMut(RtcPeerConnectionIceEvent)>,
        on_track: Closure<dyn FnMut(RtcTrackEvent)>,
        on_connection: Closure<dyn FnMut(web_sys::Event)>,
    }

    impl Drop for BrowserVoiceSession {
        fn drop(&mut self) {
            self.close_now();
        }
    }

    impl VoiceMediaSession for BrowserVoiceSession {
        fn apply_answer(&self, sdp: String) -> VoiceFuture<Result<(), String>> {
            let peer = self.peer.clone();
            Box::pin(async move {
                let answer = RtcSessionDescriptionInit::new(RtcSdpType::Answer);
                answer.set_sdp(&sdp);
                JsFuture::from(peer.set_remote_description(&answer))
                    .await
                    .map(|_| ())
                    .map_err(js_error)
            })
        }

        fn add_remote_candidate(&self, candidate: IceCandidate) -> VoiceFuture<Result<(), String>> {
            let peer = self.peer.clone();
            Box::pin(async move {
                let init = RtcIceCandidateInit::new(&candidate.candidate);
                init.set_sdp_mid(candidate.sdp_mid.as_deref());
                init.set_sdp_m_line_index(candidate.sdp_m_line_index);
                JsFuture::from(peer.add_ice_candidate_with_opt_rtc_ice_candidate_init(Some(&init)))
                    .await
                    .map(|_| ())
                    .map_err(js_error)
            })
        }

        fn finish_remote_candidates(&self) -> VoiceFuture<Result<(), String>> {
            let peer = self.peer.clone();
            Box::pin(async move {
                JsFuture::from(peer.add_ice_candidate_with_opt_rtc_ice_candidate_init(None))
                    .await
                    .map(|_| ())
                    .map_err(js_error)
            })
        }

        fn set_muted(&self, muted: bool) {
            for track in self.local_stream.get_audio_tracks().iter() {
                if let Ok(track) = track.dyn_into::<MediaStreamTrack>() {
                    track.set_enabled(!muted);
                }
            }
        }

        fn interrupt_playback(&self) {
            let _ = self.audio.pause();
        }

        fn resume_playback(&self) -> VoiceFuture<Result<(), String>> {
            let audio = self.audio.clone();
            Box::pin(async move {
                JsFuture::from(audio.play().map_err(js_error)?)
                    .await
                    .map(|_| ())
                    .map_err(js_error)
            })
        }

        fn close_now(&self) {
            self.peer.set_onicecandidate(None);
            self.peer.set_ontrack(None);
            self.peer.set_onconnectionstatechange(None);
            if let Some(BrowserCallbacks {
                on_ice,
                on_track,
                on_connection,
            }) = self.callbacks.borrow_mut().take()
            {
                drop((on_ice, on_track, on_connection));
            }
            self.peer.close();
            for track in self.local_stream.get_tracks().iter() {
                if let Ok(track) = track.dyn_into::<MediaStreamTrack>() {
                    track.stop();
                }
            }
            let _ = self.audio.pause();
            self.audio.set_src_object(None);
            self.audio.remove();
        }
    }

    impl VoiceMediaFactory for BrowserVoiceMedia {
        fn prepare(&self, callbacks: MediaCallbacks) -> VoiceFuture<Result<PreparedMedia, String>> {
            Box::pin(async move {
                let window = web_sys::window().ok_or("Browser window is unavailable.")?;
                let document = window
                    .document()
                    .ok_or("Browser document is unavailable.")?;
                let devices = window
                    .navigator()
                    .media_devices()
                    .map_err(|error| media_error("Microphone is unavailable", error))?;

                let audio_constraints = MediaTrackConstraints::new();
                audio_constraints.set_echo_cancellation(&JsValue::TRUE);
                audio_constraints.set_noise_suppression(&JsValue::TRUE);
                audio_constraints.set_auto_gain_control(&JsValue::TRUE);
                let constraints = MediaStreamConstraints::new();
                constraints.set_audio_media_track_constraints(&audio_constraints);
                constraints.set_video(&JsValue::FALSE);
                let stream = JsFuture::from(
                    devices
                        .get_user_media_with_constraints(&constraints)
                        .map_err(|error| media_error("Microphone request failed", error))?,
                )
                .await
                .map_err(|error| media_error("Microphone permission was not granted", error))?
                .dyn_into::<MediaStream>()
                .map_err(|_| "The browser returned invalid microphone media.".to_owned())?;

                let track = stream
                    .get_audio_tracks()
                    .get(0)
                    .dyn_into::<MediaStreamTrack>()
                    .map_err(|_| "The microphone did not provide an audio track.".to_owned())?;
                let settings = track.get_settings();
                let processing = AudioProcessingReport {
                    echo_cancellation: browser_setting(settings.get_echo_cancellation()),
                    noise_suppression: browser_setting(settings.get_noise_suppression()),
                    auto_gain_control: browser_setting(settings.get_auto_gain_control()),
                };

                // Empty ICE configuration is intentional for the direct same-LAN/VPN
                // release. No relay credentials are accepted or persisted here.
                let peer = RtcPeerConnection::new_with_configuration(&RtcConfiguration::new())
                    .map_err(|error| media_error("WebRTC is unavailable", error))?;
                peer.add_track_0(&track, &stream);

                let audio = document
                    .create_element("audio")
                    .map_err(js_error)?
                    .dyn_into::<HtmlAudioElement>()
                    .map_err(|_| "The browser cannot create an audio output.".to_owned())?;
                audio.set_autoplay(true);
                audio.set_attribute("playsinline", "").map_err(js_error)?;
                audio.set_hidden(true);
                document
                    .body()
                    .ok_or("Browser document body is unavailable.")?
                    .append_child(&audio)
                    .map_err(js_error)?;

                let ice_callback = callbacks.ice_candidate.clone();
                let on_ice = Closure::new(move |event: RtcPeerConnectionIceEvent| {
                    let Some(candidate) = event.candidate() else {
                        ice_callback(None);
                        return;
                    };
                    let candidate_line = candidate.candidate();
                    if !is_signal_candidate(&candidate_line) {
                        ice_callback(None);
                        return;
                    }
                    ice_callback(Some(IceCandidate {
                        candidate: candidate_line,
                        sdp_mid: candidate.sdp_mid(),
                        sdp_m_line_index: candidate.sdp_m_line_index(),
                        username_fragment: Reflect::get(
                            candidate.as_ref(),
                            &JsValue::from_str("usernameFragment"),
                        )
                        .ok()
                        .and_then(|value| value.as_string()),
                    }));
                });
                peer.set_onicecandidate(Some(on_ice.as_ref().unchecked_ref()));

                let audio_for_track = audio.clone();
                let started = callbacks.remote_audio_started.clone();
                let blocked = callbacks.playback_blocked.clone();
                let failed_for_track = callbacks.connection_failed.clone();
                let on_track = Closure::new(move |event: RtcTrackEvent| {
                    let Ok(remote) = MediaStream::new() else {
                        failed_for_track(
                            "The browser could not attach remote voice audio.".to_owned(),
                        );
                        return;
                    };
                    remote.add_track(&event.track());
                    audio_for_track.set_src_object(Some(&remote));
                    let audio = audio_for_track.clone();
                    let started = started.clone();
                    let blocked = blocked.clone();
                    wasm_bindgen_futures::spawn_local(async move {
                        let playback_started = match audio.play() {
                            Ok(promise) => JsFuture::from(promise).await.is_ok(),
                            Err(_) => false,
                        };
                        if playback_started {
                            started();
                        } else {
                            blocked();
                        }
                    });
                });
                peer.set_ontrack(Some(on_track.as_ref().unchecked_ref()));

                let peer_for_state = peer.clone();
                let failed = callbacks.connection_failed.clone();
                let interrupted = callbacks.connection_interrupted.clone();
                let recovered = callbacks.connection_recovered.clone();
                let on_connection = Closure::new(
                    move |_event: web_sys::Event| match peer_for_state.connection_state() {
                        RtcPeerConnectionState::Failed => {
                            failed("Voice is unavailable: no direct route to this Tyde host. Connect both devices to the same LAN or VPN and try again.".to_owned());
                        }
                        RtcPeerConnectionState::Disconnected => interrupted(),
                        RtcPeerConnectionState::Connected => recovered(),
                        _ => {}
                    },
                );
                peer.set_onconnectionstatechange(Some(on_connection.as_ref().unchecked_ref()));

                let offer = JsFuture::from(peer.create_offer())
                    .await
                    .map_err(|error| media_error("Could not create a voice offer", error))?
                    .dyn_into::<RtcSessionDescriptionInit>()
                    .map_err(|_| "The browser returned an invalid voice offer.".to_owned())?;
                JsFuture::from(peer.set_local_description(&offer))
                    .await
                    .map_err(|error| media_error("Could not prepare the voice offer", error))?;

                let session = Rc::new(BrowserVoiceSession {
                    peer,
                    local_stream: stream,
                    audio,
                    callbacks: RefCell::new(Some(BrowserCallbacks {
                        on_ice,
                        on_track,
                        on_connection,
                    })),
                });
                Ok(PreparedMedia {
                    offer_sdp: offer.get_sdp().unwrap_or_default(),
                    processing,
                    session,
                })
            })
        }
    }

    fn browser_setting(value: Option<bool>) -> BrowserAudioSetting {
        match value {
            Some(true) => BrowserAudioSetting::Enabled,
            Some(false) => BrowserAudioSetting::Disabled,
            None => BrowserAudioSetting::Unavailable,
        }
    }

    fn js_error(error: JsValue) -> String {
        error
            .as_string()
            .unwrap_or_else(|| "browser media operation failed".to_owned())
    }

    fn media_error(context: &str, error: JsValue) -> String {
        format!("{context}: {}", js_error(error))
    }
}

#[cfg(target_arch = "wasm32")]
pub use browser::BrowserVoiceMedia;

#[cfg(test)]
mod tests {
    use super::is_signal_candidate;

    #[test]
    fn end_of_candidates_is_not_queued_as_a_candidate() {
        assert!(!is_signal_candidate(""));
        assert!(!is_signal_candidate("end-of-candidates"));
        assert!(is_signal_candidate(
            "candidate:1 1 UDP 1 192.0.2.1 5000 typ host"
        ));
    }
}
