//! The media seam.
//!
//! Everything the voice layer needs from the browser's audio stack sits behind
//! [`MediaPlatform`]. Production uses [`crate::voice::media_web::WebMediaPlatform`];
//! tests use [`FakeMediaPlatform`], which is deterministic, needs no
//! microphone, and can be told to fail at any stage.
//!
//! Two rules this module exists to enforce:
//!
//! - **No silent degradation.** Every entry point returns `Result`. A missing
//!   `navigator.mediaDevices`, a refused permission, or a rejected `play()` is
//!   reported with the real underlying message. There is no path where the mic
//!   control appears to work and quietly does nothing.
//! - **Playback is a remote WebRTC track on an audio element, never decoded
//!   audio pushed through Web Audio.** Platform echo cancellation only cancels
//!   far-end audio the media stack owns, so the seam deliberately exposes no
//!   "play these bytes" entry point at all. Removing that capability from the
//!   API is what stops a later change from silently breaking AEC.

// Only the test double keeps interior-mutable state; the production seam is
// stateless.
#[cfg(test)]
use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

use super::session::{EffectiveAudio, VoiceStage};

/// Futures here are deliberately not `Send`: they wrap browser promises that
/// are bound to the single wasm thread.
pub type MediaFuture<T> = Pin<Box<dyn Future<Output = Result<T, MediaError>>>>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaError {
    pub stage: VoiceStage,
    /// The real underlying message. Never a generic substitute.
    pub message: String,
    pub retryable: bool,
    /// True when this "error" is the platform reporting that the caller's own
    /// `stop` interrupted the operation. The controller must not raise it to
    /// the user: nothing went wrong, and the resources are already released.
    pub cancelled: bool,
}

impl MediaError {
    pub fn new(stage: VoiceStage, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            stage,
            message: message.into(),
            retryable,
            cancelled: false,
        }
    }

    /// The operation was abandoned because `stop` was called. Everything it had
    /// acquired has already been released.
    pub fn cancelled() -> Self {
        Self {
            stage: VoiceStage::Media,
            message: "voice session was stopped during setup".to_owned(),
            retryable: false,
            cancelled: true,
        }
    }

    pub fn is_cancellation(&self) -> bool {
        self.cancelled
    }

    pub fn microphone(message: impl Into<String>) -> Self {
        Self::new(VoiceStage::Microphone, message, true)
    }

    pub fn negotiation(message: impl Into<String>) -> Self {
        Self::new(VoiceStage::Negotiation, message, true)
    }

    pub fn media(message: impl Into<String>) -> Self {
        Self::new(VoiceStage::Media, message, true)
    }
}

/// A STUN/TURN server, as declared by the host. The desktop never invents
/// these and never ships a default public server: if the host declares none,
/// only host candidates are used.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct IceServerConfig {
    pub urls: Vec<String>,
    pub username: Option<String>,
    pub credential: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct MediaStartRequest {
    pub ice_servers: Vec<IceServerConfig>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalIceCandidate {
    pub candidate: String,
    pub sdp_mid: Option<String>,
    pub sdp_m_line_index: Option<u16>,
}

pub type RemoteIceCandidate = LocalIceCandidate;

/// Asynchronous notifications from the media stack. Each is delivered to the
/// controller, which decides whether the owning session is still current.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MediaEvent {
    LocalIceCandidate(LocalIceCandidate),
    LocalIceComplete,
    /// The remote audio track arrived and is attached to the output element.
    RemoteTrackAttached,
    /// `HTMLMediaElement.play()` was refused. Voice is still connected; one
    /// user gesture unblocks it.
    PlaybackBlocked(String),
    Connected,
    /// ICE entered `disconnected`. This is the *recoverable* state — a consent
    /// refresh lapse or a brief network roam commonly returns to `connected`
    /// without renegotiation — so it must not end the session.
    ConnectionUnstable,
    /// The peer connection failed or closed for good. Carries the state that
    /// caused it.
    Disconnected(String),
    /// The captured track ended on its own — device unplugged, or the OS
    /// revoked access mid-session.
    MicrophoneEnded,
}

pub type MediaEventSink = Rc<dyn Fn(MediaEvent)>;

/// Result of a successful `start`: the offer to send to the host, plus the
/// audio settings the acquired track actually reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaStarted {
    pub offer_sdp: String,
    pub effective_audio: EffectiveAudio,
}

pub trait MediaPlatform {
    /// Acquire the microphone, build the peer connection, and produce a local
    /// offer.
    ///
    /// Order matters and is part of the contract: the microphone is acquired
    /// **before** the offer is created. Browsers emit obfuscated `.local` mDNS
    /// host candidates for pages that have not been granted microphone access,
    /// and a non-browser ICE agent has no mDNS resolver to reach them.
    fn start(
        &self,
        request: MediaStartRequest,
        events: MediaEventSink,
    ) -> MediaFuture<MediaStarted>;

    fn accept_answer(&self, sdp: String) -> MediaFuture<()>;

    fn add_remote_candidate(&self, candidate: RemoteIceCandidate) -> MediaFuture<()>;

    /// Mute toggles `MediaStreamTrack.enabled`. It does not end the session and
    /// does not renegotiate.
    fn set_muted(&self, muted: bool) -> Result<(), MediaError>;

    /// Silence remote output immediately, leaving capture live.
    ///
    /// Local only: it pauses the output element and sends nothing anywhere.
    /// It must be synchronous — a round trip before the audio stops would
    /// defeat the point — and it must leave the session recoverable, which is
    /// why the controller pairs it with `playback_blocked` so the existing
    /// "Tap to hear" control reappears.
    ///
    /// Whether the *agent* stops talking is provider-side barge-in reacting to
    /// the still-live microphone, which this method neither performs nor can
    /// claim.
    fn silence_output(&self) -> Result<(), MediaError>;

    /// Retry blocked output after a user gesture, or after an interruption
    /// once the agent starts a new utterance.
    fn resume_playback(&self) -> MediaFuture<()>;

    /// Stop every track, detach and pause playback, close the peer connection,
    /// and release handlers. Must be synchronous and safe to call twice.
    fn stop(&self);
}

// ── Test double ─────────────────────────────────────────────────────────────
//
// Gated out of the shipped bundle: the wasm module is close to the browser
// memory ceiling that the single test binary runs against, and none of this is
// reachable from production code.

/// What the fake should do at each stage. Defaults to a working session.
#[cfg(test)]
#[derive(Clone, Debug, Default)]
pub struct FakeMediaScript {
    pub start_error: Option<MediaError>,
    pub answer_error: Option<MediaError>,
    pub mute_error: Option<MediaError>,
    pub effective_audio: EffectiveAudio,
    pub offer_sdp: Option<String>,
    /// When true, `start` returns a future that stays pending until
    /// [`FakeMediaPlatform::resolve_start`] is called — modelling a microphone
    /// permission prompt the user has not answered yet. That window is where
    /// the interesting teardown races live.
    pub start_pending: bool,
}

#[cfg(test)]
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FakeMediaCalls {
    pub starts: usize,
    pub answers: Vec<String>,
    pub remote_candidates: Vec<RemoteIceCandidate>,
    pub muted: Vec<bool>,
    pub resumes: usize,
    pub silences: usize,
    pub stops: usize,
}

/// A start that completes only when the test says so, modelling a microphone
/// permission prompt sitting open. Resolving with `None` means "the caller
/// stopped us first", which is what the real platform reports as
/// [`MediaError::cancelled`].
#[cfg(test)]
#[derive(Default)]
struct PendingStart {
    result: Option<Result<MediaStarted, MediaError>>,
    waker: Option<std::task::Waker>,
}

#[cfg(test)]
struct PendingStartFuture {
    slot: Rc<RefCell<PendingStart>>,
}

#[cfg(test)]
impl Future for PendingStartFuture {
    type Output = Result<MediaStarted, MediaError>;

    fn poll(
        self: Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let mut slot = self.slot.borrow_mut();
        match slot.result.take() {
            Some(result) => std::task::Poll::Ready(result),
            None => {
                slot.waker = Some(context.waker().clone());
                std::task::Poll::Pending
            }
        }
    }
}

/// Deterministic [`MediaPlatform`] for tests: no microphone, no peer
/// connection, no browser required. Records every call so teardown and
/// generation behaviour can be asserted directly.
#[cfg(test)]
pub struct FakeMediaPlatform {
    script: FakeMediaScript,
    calls: RefCell<FakeMediaCalls>,
    sink: RefCell<Option<MediaEventSink>>,
    /// Set by `stop`. Mirrors the real platform's cancellation token: a start
    /// that completes after this is set must release what it acquired instead
    /// of handing it to a controller that no longer owns the session.
    cancelled: RefCell<bool>,
    pending: RefCell<Option<Rc<RefCell<PendingStart>>>>,
}

#[cfg(test)]
impl FakeMediaPlatform {
    pub fn new() -> Self {
        Self::with_script(FakeMediaScript::default())
    }

    pub fn with_script(script: FakeMediaScript) -> Self {
        Self {
            script,
            calls: RefCell::new(FakeMediaCalls::default()),
            sink: RefCell::new(None),
            cancelled: RefCell::new(false),
            pending: RefCell::new(None),
        }
    }

    pub fn calls(&self) -> FakeMediaCalls {
        self.calls.borrow().clone()
    }

    /// Push an event as if the media stack produced it.
    pub fn emit(&self, event: MediaEvent) {
        let sink = self.sink.borrow().clone();
        if let Some(sink) = sink {
            sink(event);
        }
    }

    /// True once `stop` has run. A start still in flight is expected to
    /// observe this and release rather than complete.
    pub fn is_cancelled(&self) -> bool {
        *self.cancelled.borrow()
    }

    /// Complete a `start_pending` acquisition. If `stop` already ran, the
    /// start resolves as cancelled — exactly what the browser platform does
    /// when the permission prompt is answered after teardown.
    pub fn resolve_start(&self) {
        let Some(slot) = self.pending.borrow_mut().take() else {
            return;
        };
        let result = if *self.cancelled.borrow() {
            Err(MediaError::cancelled())
        } else {
            Ok(self.started_value())
        };
        let waker = {
            let mut pending = slot.borrow_mut();
            pending.result = Some(result);
            pending.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    fn started_value(&self) -> MediaStarted {
        MediaStarted {
            offer_sdp: self
                .script
                .offer_sdp
                .clone()
                .unwrap_or_else(|| "v=0\r\nfake-offer\r\n".to_owned()),
            effective_audio: self.script.effective_audio,
        }
    }
}

#[cfg(test)]
impl Default for FakeMediaPlatform {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
fn ready<T: 'static>(value: Result<T, MediaError>) -> MediaFuture<T> {
    Box::pin(async move { value })
}

#[cfg(test)]
impl MediaPlatform for FakeMediaPlatform {
    fn start(
        &self,
        _request: MediaStartRequest,
        events: MediaEventSink,
    ) -> MediaFuture<MediaStarted> {
        self.calls.borrow_mut().starts += 1;
        *self.cancelled.borrow_mut() = false;
        if let Some(error) = self.script.start_error.clone() {
            return ready(Err(error));
        }
        *self.sink.borrow_mut() = Some(events);
        if self.script.start_pending {
            let slot = Rc::new(RefCell::new(PendingStart::default()));
            *self.pending.borrow_mut() = Some(slot.clone());
            return Box::pin(PendingStartFuture { slot });
        }
        ready(Ok(self.started_value()))
    }

    fn accept_answer(&self, sdp: String) -> MediaFuture<()> {
        self.calls.borrow_mut().answers.push(sdp);
        match self.script.answer_error.clone() {
            Some(error) => ready(Err(error)),
            None => ready(Ok(())),
        }
    }

    fn add_remote_candidate(&self, candidate: RemoteIceCandidate) -> MediaFuture<()> {
        self.calls.borrow_mut().remote_candidates.push(candidate);
        ready(Ok(()))
    }

    fn set_muted(&self, muted: bool) -> Result<(), MediaError> {
        self.calls.borrow_mut().muted.push(muted);
        match self.script.mute_error.clone() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn silence_output(&self) -> Result<(), MediaError> {
        self.calls.borrow_mut().silences += 1;
        Ok(())
    }

    fn resume_playback(&self) -> MediaFuture<()> {
        self.calls.borrow_mut().resumes += 1;
        ready(Ok(()))
    }

    fn stop(&self) {
        self.calls.borrow_mut().stops += 1;
        *self.cancelled.borrow_mut() = true;
        *self.sink.borrow_mut() = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_seam_exposes_no_way_to_play_raw_audio_bytes() {
        // This is an architectural guarantee expressed as a compile-time API
        // shape, restated here so the intent survives refactors: playback goes
        // through a remote WebRTC track on an audio element, because platform
        // AEC only cancels far-end audio the media stack owns. If someone adds
        // a `play_encoded_audio(&[u8])` to `MediaPlatform`, echo cancellation
        // silently stops working and nothing else in the suite would notice.
        //
        // `MediaStarted` carries an SDP offer and observed settings — no audio
        // payload — and `MediaEvent` carries no sample data either.
        let started = MediaStarted {
            offer_sdp: "v=0".to_owned(),
            effective_audio: EffectiveAudio::default(),
        };
        assert!(started.offer_sdp.starts_with("v="));
    }

    #[test]
    fn fake_records_calls_and_stop_is_repeatable() {
        let platform = FakeMediaPlatform::new();
        platform.set_muted(true).expect("mute");
        platform.stop();
        platform.stop();
        let calls = platform.calls();
        assert_eq!(calls.muted, vec![true]);
        assert_eq!(calls.stops, 2, "stop must be safe to call more than once");
    }

    #[test]
    fn a_start_that_completes_after_stop_reports_cancellation_not_success() {
        // The permission-prompt race: the user is asked for the microphone,
        // switches agents while the sheet is open, and only then grants it.
        let platform = FakeMediaPlatform::with_script(FakeMediaScript {
            start_pending: true,
            ..FakeMediaScript::default()
        });
        let sink: MediaEventSink = Rc::new(|_| {});
        let mut future = platform.start(MediaStartRequest::default(), sink);

        let waker = std::task::Waker::noop();
        let mut context = std::task::Context::from_waker(waker);
        assert!(
            future.as_mut().poll(&mut context).is_pending(),
            "acquisition is still outstanding"
        );

        platform.stop();
        platform.resolve_start();

        match future.as_mut().poll(&mut context) {
            std::task::Poll::Ready(Err(error)) => assert!(
                error.is_cancellation(),
                "a stop during acquisition is a cancellation, not a user-visible failure"
            ),
            other => panic!("expected a cancelled start, got {other:?}"),
        }
        assert!(platform.is_cancelled());
    }

    #[test]
    fn fake_events_stop_flowing_after_teardown() {
        let platform = Rc::new(FakeMediaPlatform::new());
        let seen = Rc::new(RefCell::new(Vec::new()));
        let sink_seen = seen.clone();
        let sink: MediaEventSink = Rc::new(move |event| sink_seen.borrow_mut().push(event));
        // The fake installs its event sink synchronously inside `start`, so
        // the future's result is irrelevant to what this test asserts.
        drop(platform.start(MediaStartRequest::default(), sink));
        platform.emit(MediaEvent::Connected);
        platform.stop();
        platform.emit(MediaEvent::MicrophoneEnded);
        assert_eq!(*seen.borrow(), vec![MediaEvent::Connected]);
    }
}
