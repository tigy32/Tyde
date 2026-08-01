//! Browser implementation of the [`MediaPlatform`] seam.
//!
//! Compiled only for wasm. Everything here is plumbing around four browser
//! objects — a `MediaStream`, its audio `MediaStreamTrack`, an
//! `RTCPeerConnection`, and an `HTMLAudioElement` — and the closures that keep
//! them alive. None of those are `Send + Sync`, which is why the whole runtime
//! lives behind an `Rc<RefCell<…>>` owned by a thread-local rather than in
//! `AppState` or a Leptos signal.
//!
//! Ordering that matters:
//!
//! - The microphone is acquired **before** the offer is created. A page that
//!   has not been granted microphone access gets obfuscated `.local` mDNS host
//!   candidates, which a non-browser ICE agent cannot resolve.
//! - Remote audio is attached to an `<audio>` element as a live WebRTC track.
//!   It is never decoded and pushed through Web Audio, because platform echo
//!   cancellation only cancels far-end audio the media stack owns.

use std::cell::RefCell;
use std::rc::Rc;

use js_sys::{Array, Reflect};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    HtmlAudioElement, MediaStream, MediaStreamConstraints, MediaStreamTrack, MediaTrackConstraints,
    RtcConfiguration, RtcIceCandidateInit, RtcPeerConnection, RtcPeerConnectionIceEvent,
    RtcPeerConnectionState, RtcSdpType, RtcSessionDescriptionInit, RtcTrackEvent,
};

use super::media::{
    IceServerConfig, LocalIceCandidate, MediaError, MediaEvent, MediaEventSink, MediaFuture,
    MediaPlatform, MediaStartRequest, MediaStarted, RemoteIceCandidate,
};
use super::session::EffectiveAudio;

/// The live browser objects for one session, populated incrementally.
///
/// Every field is optional because construction is interleaved with `await`
/// points — `getUserMedia`, `createOffer`, `setLocalDescription` — and a
/// teardown can land in any of those gaps. Registering each object here the
/// moment it exists is what makes `stop` able to release a half-built session;
/// waiting until the end and assigning a fully-formed struct leaves an
/// unreachable live microphone whenever the user switches agents mid-prompt.
#[derive(Default)]
struct WebSession {
    /// Set by `stop`. A construction still in flight polls this after every
    /// `await` and releases instead of completing.
    cancelled: bool,
    stream: Option<MediaStream>,
    track: Option<MediaStreamTrack>,
    peer: Option<RtcPeerConnection>,
    audio: Option<HtmlAudioElement>,
    on_ice_candidate: Option<Closure<dyn FnMut(RtcPeerConnectionIceEvent)>>,
    on_track: Option<Closure<dyn FnMut(RtcTrackEvent)>>,
    on_connection_state: Option<Closure<dyn FnMut(JsValue)>>,
    on_track_ended: Option<Closure<dyn FnMut(JsValue)>>,
}

impl WebSession {
    /// Release everything currently held. Safe to call repeatedly and at any
    /// stage of construction.
    fn release(&mut self) {
        // Detach every handler *first*. Stopping a track fires `ended` and
        // closing a peer connection fires a `closed` state change; if those
        // still reach the controller they look exactly like a device
        // disappearing or a network drop, and the user gets an error banner
        // for a teardown they asked for.
        if let Some(track) = self.track.as_ref() {
            track.set_onended(None);
        }
        if let Some(peer) = self.peer.as_ref() {
            peer.set_onicecandidate(None);
            peer.set_ontrack(None);
            peer.set_onconnectionstatechange(None);
        }
        self.on_ice_candidate = None;
        self.on_track = None;
        self.on_connection_state = None;
        self.on_track_ended = None;

        // Then release capture, so the OS microphone indicator clears as
        // promptly as the platform allows.
        if let Some(stream) = self.stream.take() {
            for track in stream.get_tracks().iter() {
                if let Ok(track) = track.dyn_into::<MediaStreamTrack>() {
                    track.stop();
                }
            }
        }
        if let Some(track) = self.track.take() {
            track.stop();
        }

        if let Some(audio) = self.audio.take() {
            let _ = audio.pause();
            audio.set_src_object(None);
            audio.remove();
        }
        if let Some(peer) = self.peer.take() {
            peer.close();
        }
    }
}

#[derive(Default)]
pub struct WebMediaPlatform {
    session: Rc<RefCell<WebSession>>,
}

impl WebMediaPlatform {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Abandon a construction whose session was stopped while it was suspended,
/// releasing whatever it had already acquired.
fn abandon(slot: &Rc<RefCell<WebSession>>) -> MediaError {
    slot.borrow_mut().release();
    MediaError::cancelled()
}

/// Fail a construction, releasing whatever it had already acquired. Without
/// this, an error after `getUserMedia` succeeds would leave a live microphone
/// with no owner — dropping a `MediaStream` wrapper does not stop its tracks.
fn abort(slot: &Rc<RefCell<WebSession>>, error: MediaError) -> MediaError {
    slot.borrow_mut().release();
    error
}

fn js_message(error: &JsValue) -> String {
    if let Some(text) = error.as_string() {
        let text = text.trim();
        if !text.is_empty() {
            return text.to_owned();
        }
    }
    if let Some(error) = error.dyn_ref::<js_sys::Error>() {
        let name = error.name().as_string().unwrap_or_default();
        let message = error.message().as_string().unwrap_or_default();
        let message = message.trim();
        return match (name.trim().is_empty(), message.is_empty()) {
            (false, false) => format!("{}: {}", name.trim(), message),
            (true, false) => message.to_owned(),
            (false, true) => name.trim().to_owned(),
            (true, true) => "the browser reported an error with no detail".to_owned(),
        };
    }
    // Reflect one level for DOMException-shaped values that are not `Error`.
    if let Ok(message) = Reflect::get(error, &JsValue::from_str("message"))
        && let Some(message) = message.as_string()
        && !message.trim().is_empty()
    {
        return message.trim().to_owned();
    }
    "the browser reported an error with no detail".to_owned()
}

fn read_bool(settings: &JsValue, key: &str) -> Option<bool> {
    Reflect::get(settings, &JsValue::from_str(key))
        .ok()
        .and_then(|value| value.as_bool())
}

fn read_u32(settings: &JsValue, key: &str) -> Option<u32> {
    Reflect::get(settings, &JsValue::from_str(key))
        .ok()
        .and_then(|value| value.as_f64())
        .map(|value| value as u32)
}

/// Read back what the acquired track actually applied.
///
/// `MediaTrackSettings` is a plain JS dictionary, and a browser is free to
/// omit any key. Absent is reported as `None` — deliberately not as `false`,
/// which would let the UI claim echo cancellation is off when the truth is
/// that the browser did not say.
fn effective_audio(track: &MediaStreamTrack) -> EffectiveAudio {
    let settings: JsValue = track.get_settings().into();
    EffectiveAudio {
        echo_cancellation: read_bool(&settings, "echoCancellation"),
        noise_suppression: read_bool(&settings, "noiseSuppression"),
        auto_gain_control: read_bool(&settings, "autoGainControl"),
        sample_rate: read_u32(&settings, "sampleRate"),
    }
}

fn ice_server_value(servers: &[IceServerConfig]) -> Array {
    let list = Array::new();
    for server in servers {
        let entry = js_sys::Object::new();
        let urls = Array::new();
        for url in &server.urls {
            urls.push(&JsValue::from_str(url));
        }
        let _ = Reflect::set(&entry, &JsValue::from_str("urls"), &urls);
        if let Some(username) = &server.username {
            let _ = Reflect::set(
                &entry,
                &JsValue::from_str("username"),
                &JsValue::from_str(username),
            );
        }
        if let Some(credential) = &server.credential {
            let _ = Reflect::set(
                &entry,
                &JsValue::from_str("credential"),
                &JsValue::from_str(credential),
            );
        }
        list.push(&entry);
    }
    list
}

/// The hidden element remote audio is rendered through.
///
/// It is attached to the document because WKWebView will not reliably start
/// playback for a detached media element, and `autoplay` is set because the
/// desktop shell already permits it (wry enables autoplay by default).
fn create_audio_element() -> Result<HtmlAudioElement, MediaError> {
    let audio = HtmlAudioElement::new().map_err(|error| {
        MediaError::media(format!(
            "could not create the audio output: {}",
            js_message(&error)
        ))
    })?;
    audio.set_autoplay(true);
    let _ = audio.set_attribute("data-test", "voice-remote-audio");
    let _ = audio.style().set_property("display", "none");
    if let Some(body) = web_sys::window()
        .and_then(|window| window.document())
        .and_then(|document| document.body())
    {
        let _ = body.append_child(&audio);
    }
    Ok(audio)
}

async fn acquire_microphone() -> Result<(MediaStream, MediaStreamTrack), MediaError> {
    let window = web_sys::window().ok_or_else(|| MediaError::microphone("no browser window"))?;
    let devices = window.navigator().media_devices().map_err(|error| {
        // This is the packaged-desktop failure mode: a non-secure context has
        // no `navigator.mediaDevices` at all. Say so instead of implying the
        // user refused permission.
        MediaError::new(
            super::session::VoiceStage::Microphone,
            format!(
                "This build cannot reach the microphone API ({}). Audio capture needs a secure context.",
                js_message(&error)
            ),
            false,
        )
    })?;

    let audio = MediaTrackConstraints::new();
    audio.set_echo_cancellation(&JsValue::TRUE);
    audio.set_noise_suppression(&JsValue::TRUE);
    audio.set_auto_gain_control(&JsValue::TRUE);
    let constraints = MediaStreamConstraints::new();
    constraints.set_audio(&audio);
    constraints.set_video(&JsValue::FALSE);

    let promise = devices
        .get_user_media_with_constraints(&constraints)
        .map_err(|error| {
            MediaError::microphone(format!(
                "could not request the microphone: {}",
                js_message(&error)
            ))
        })?;
    let stream: MediaStream = JsFuture::from(promise)
        .await
        .map_err(|error| MediaError::microphone(js_message(&error)))?
        .dyn_into()
        .map_err(|_| MediaError::microphone("the browser did not return an audio stream"))?;

    let track: MediaStreamTrack = stream
        .get_audio_tracks()
        .get(0)
        .dyn_into()
        .map_err(|_| MediaError::microphone("the captured stream has no audio track"))?;

    Ok((stream, track))
}

impl MediaPlatform for WebMediaPlatform {
    fn start(
        &self,
        request: MediaStartRequest,
        events: MediaEventSink,
    ) -> MediaFuture<MediaStarted> {
        let slot = self.session.clone();
        // A fresh construction clears the cancellation flag left by any
        // previous session, and releases anything a previous session somehow
        // still holds, so a start can never inherit foreign handles.
        {
            let mut session = slot.borrow_mut();
            session.release();
            session.cancelled = false;
        }
        Box::pin(async move {
            // Microphone first — see the module comment on mDNS candidates.
            let (stream, track) = acquire_microphone().await?;

            // Register both immediately, before anything else can fail or
            // suspend. From here on, `stop` can release them.
            {
                let mut session = slot.borrow_mut();
                if session.cancelled {
                    // Teardown happened while the permission sheet was open.
                    // Park them so `release` stops them, then bail.
                    session.stream = Some(stream);
                    session.track = Some(track);
                    drop(session);
                    return Err(abandon(&slot));
                }
                session.stream = Some(stream.clone());
                session.track = Some(track.clone());
            }
            let audio_settings = effective_audio(&track);

            let configuration = RtcConfiguration::new();
            configuration.set_ice_servers(&ice_server_value(&request.ice_servers));
            let peer = match RtcPeerConnection::new_with_configuration(&configuration) {
                Ok(peer) => peer,
                Err(error) => {
                    return Err(abort(
                        &slot,
                        MediaError::negotiation(format!(
                            "could not create the audio connection: {}",
                            js_message(&error)
                        )),
                    ));
                }
            };
            // Store the peer before anything else can fail. Creating the audio
            // element can throw, and a peer that is still only a local
            // binding at that point is unreachable from `release` — capture
            // would be stopped but the connection left open.
            slot.borrow_mut().peer = Some(peer.clone());

            let audio = match create_audio_element() {
                Ok(audio) => audio,
                Err(error) => return Err(abort(&slot, error)),
            };
            slot.borrow_mut().audio = Some(audio.clone());

            let ice_events = events.clone();
            let on_ice_candidate = Closure::<dyn FnMut(RtcPeerConnectionIceEvent)>::new(
                move |event: RtcPeerConnectionIceEvent| {
                    match event.candidate() {
                        Some(candidate) => {
                            ice_events(MediaEvent::LocalIceCandidate(LocalIceCandidate {
                                candidate: candidate.candidate(),
                                sdp_mid: candidate.sdp_mid(),
                                sdp_m_line_index: candidate.sdp_m_line_index(),
                            }))
                        }
                        // A null candidate is the end-of-gathering signal.
                        None => ice_events(MediaEvent::LocalIceComplete),
                    }
                },
            );
            peer.set_onicecandidate(Some(on_ice_candidate.as_ref().unchecked_ref()));

            let track_events = events.clone();
            let track_audio = audio.clone();
            let on_track = Closure::<dyn FnMut(RtcTrackEvent)>::new(move |event: RtcTrackEvent| {
                let remote = event
                    .streams()
                    .get(0)
                    .dyn_into::<MediaStream>()
                    .ok()
                    .or_else(|| {
                        let stream = MediaStream::new().ok()?;
                        stream.add_track(&event.track());
                        Some(stream)
                    });
                let Some(remote) = remote else {
                    track_events(MediaEvent::PlaybackBlocked(
                        "the remote audio track could not be attached".to_owned(),
                    ));
                    return;
                };
                track_audio.set_src_object(Some(&remote));
                track_events(MediaEvent::RemoteTrackAttached);
                match track_audio.play() {
                    Ok(promise) => {
                        let blocked_events = track_events.clone();
                        wasm_bindgen_futures::spawn_local(async move {
                            if let Err(error) = JsFuture::from(promise).await {
                                blocked_events(MediaEvent::PlaybackBlocked(js_message(&error)));
                            }
                        });
                    }
                    Err(error) => {
                        track_events(MediaEvent::PlaybackBlocked(js_message(&error)));
                    }
                }
            });
            peer.set_ontrack(Some(on_track.as_ref().unchecked_ref()));

            let state_events = events.clone();
            let state_peer = peer.clone();
            let on_connection_state = Closure::<dyn FnMut(JsValue)>::new(move |_: JsValue| {
                // `disconnected` is the *recoverable* ICE state — a consent
                // refresh lapse or a brief roam commonly returns to
                // `connected` without renegotiation. Only `failed` and
                // `closed` are terminal, so `disconnected` is reported
                // separately and the controller treats it as a warning.
                match state_peer.connection_state() {
                    RtcPeerConnectionState::Connected => state_events(MediaEvent::Connected),
                    RtcPeerConnectionState::Disconnected => {
                        state_events(MediaEvent::ConnectionUnstable)
                    }
                    RtcPeerConnectionState::Failed => {
                        state_events(MediaEvent::Disconnected("failed".to_owned()))
                    }
                    RtcPeerConnectionState::Closed => {
                        state_events(MediaEvent::Disconnected("closed".to_owned()))
                    }
                    _ => {}
                }
            });
            peer.set_onconnectionstatechange(Some(on_connection_state.as_ref().unchecked_ref()));

            let ended_events = events.clone();
            let on_track_ended = Closure::<dyn FnMut(JsValue)>::new(move |_: JsValue| {
                ended_events(MediaEvent::MicrophoneEnded);
            });
            track.set_onended(Some(on_track_ended.as_ref().unchecked_ref()));

            {
                let mut session = slot.borrow_mut();
                session.on_ice_candidate = Some(on_ice_candidate);
                session.on_track = Some(on_track);
                session.on_connection_state = Some(on_connection_state);
                session.on_track_ended = Some(on_track_ended);
                if session.cancelled {
                    drop(session);
                    return Err(abandon(&slot));
                }
            }

            peer.add_track(&track, &stream, &Array::new());

            let offer = match JsFuture::from(peer.create_offer()).await {
                Ok(offer) => offer,
                Err(error) => {
                    return Err(abort(
                        &slot,
                        MediaError::negotiation(format!(
                            "could not create the audio offer: {}",
                            js_message(&error)
                        )),
                    ));
                }
            };
            // Read the flag out before acting on it. `abandon` takes a
            // mutable borrow of the same cell, and relying on when a
            // temporary in an `if` condition is released is not something
            // this path — which the host-target check never compiles — should
            // depend on.
            let cancelled = slot.borrow().cancelled;
            if cancelled {
                return Err(abandon(&slot));
            }
            let Some(sdp) = Reflect::get(&offer, &JsValue::from_str("sdp"))
                .ok()
                .and_then(|value| value.as_string())
            else {
                return Err(abort(
                    &slot,
                    MediaError::negotiation("the browser produced an offer with no SDP"),
                ));
            };

            let description = RtcSessionDescriptionInit::new(RtcSdpType::Offer);
            description.set_sdp(&sdp);
            if let Err(error) = JsFuture::from(peer.set_local_description(&description)).await {
                return Err(abort(
                    &slot,
                    MediaError::negotiation(format!(
                        "could not apply the local audio description: {}",
                        js_message(&error)
                    )),
                ));
            }
            // Read the flag out before acting on it. `abandon` takes a
            // mutable borrow of the same cell, and relying on when a
            // temporary in an `if` condition is released is not something
            // this path — which the host-target check never compiles — should
            // depend on.
            let cancelled = slot.borrow().cancelled;
            if cancelled {
                return Err(abandon(&slot));
            }

            Ok(MediaStarted {
                offer_sdp: sdp,
                effective_audio: audio_settings,
            })
        })
    }

    fn accept_answer(&self, sdp: String) -> MediaFuture<()> {
        let peer = self.session.borrow().peer.clone();
        Box::pin(async move {
            let peer =
                peer.ok_or_else(|| MediaError::negotiation("the audio session is no longer open"))?;
            let description = RtcSessionDescriptionInit::new(RtcSdpType::Answer);
            description.set_sdp(&sdp);
            JsFuture::from(peer.set_remote_description(&description))
                .await
                .map_err(|error| {
                    MediaError::negotiation(format!(
                        "the host's audio answer was rejected: {}",
                        js_message(&error)
                    ))
                })?;
            Ok(())
        })
    }

    fn add_remote_candidate(&self, candidate: RemoteIceCandidate) -> MediaFuture<()> {
        let peer = self.session.borrow().peer.clone();
        Box::pin(async move {
            // A candidate arriving after teardown is normal, not an error: the
            // host may have sent it before it saw our stop.
            let Some(peer) = peer else {
                return Ok(());
            };
            let init = RtcIceCandidateInit::new(&candidate.candidate);
            init.set_sdp_mid(candidate.sdp_mid.as_deref());
            init.set_sdp_m_line_index(candidate.sdp_m_line_index);
            JsFuture::from(peer.add_ice_candidate_with_opt_rtc_ice_candidate_init(Some(&init)))
                .await
                .map_err(|error| {
                    MediaError::negotiation(format!(
                        "a host audio candidate was rejected: {}",
                        js_message(&error)
                    ))
                })?;
            Ok(())
        })
    }

    fn set_muted(&self, muted: bool) -> Result<(), MediaError> {
        match self.session.borrow().track.as_ref() {
            Some(track) => {
                track.set_enabled(!muted);
                Ok(())
            }
            // Reachable: the user can hit Mute while the permission sheet is
            // still open. Report it, but the caller must treat it as a warning
            // — the session is still coming up and must stay endable.
            None => Err(MediaError::media(
                "the microphone is not open yet, so it cannot be muted",
            )),
        }
    }

    fn silence_output(&self) -> Result<(), MediaError> {
        match self.session.borrow().audio.as_ref() {
            Some(audio) => {
                // Pause only. The `srcObject` stays attached and capture stays
                // live, so `resume_playback` restores audio without
                // renegotiating and the user can keep talking through it.
                audio
                    .pause()
                    .map_err(|error| MediaError::media(js_message(&error)))?;
                Ok(())
            }
            None => Err(MediaError::media("there is no audio output to silence")),
        }
    }

    fn resume_playback(&self) -> MediaFuture<()> {
        let audio = self.session.borrow().audio.clone();
        Box::pin(async move {
            let audio =
                audio.ok_or_else(|| MediaError::media("the audio session is no longer open"))?;
            let promise = audio
                .play()
                .map_err(|error| MediaError::media(js_message(&error)))?;
            JsFuture::from(promise)
                .await
                .map_err(|error| MediaError::media(js_message(&error)))?;
            Ok(())
        })
    }

    fn stop(&self) {
        // Mark cancelled *and* release. The flag is what a construction still
        // suspended on `getUserMedia` observes when it resumes; the release
        // handles everything already acquired. Safe to call repeatedly.
        let mut session = self.session.borrow_mut();
        session.cancelled = true;
        session.release();
    }
}
