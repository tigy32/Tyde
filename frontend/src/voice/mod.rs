//! Desktop voice layer.
//!
//! One voice session at a time, bound to one focused agent, layered over the
//! ordinary chat without replacing any of it.
//!
//! Module map:
//!
//! - [`session`] — the pure state machine. Target binding, generation guards,
//!   teardown idempotence. No browser, no protocol.
//! - [`media`] — the [`media::MediaPlatform`] seam plus its deterministic fake.
//! - [`media_web`] — the browser implementation (wasm only).
//! - [`wire`] — the only module that names `protocol::Voice*` types.
//! - This file — the controller that ties them together.
//!
//! ## Why a thread-local and not `AppState`
//!
//! A live session owns a `MediaStream`, an `RTCPeerConnection`, an
//! `HTMLAudioElement`, and several `Closure`s. None are `Send + Sync`, so they
//! cannot go in `AppState` or in a Leptos signal. The repository already
//! solves this shape with thread-locals — `term_bridge::HANDLES`,
//! `header::USER_FACING_ERROR`, `dispatch::INBOUND_SEQ` — and this follows
//! that pattern: reactive plain data in an `ArcRwSignal`, live browser handles
//! in a `RefCell` beside it.
//!
//! ## Why there is no silent degradation
//!
//! Every failure path ends in a visible state: a `VoicePhase::Failed` with the
//! real underlying message, reported through `header::report_user_error` as
//! well. There is no branch where the microphone control appears to work and
//! quietly does nothing.

pub mod media;
// Not `cfg`-gated. `web-sys` and `wasm-bindgen` compile for the host, and the
// rest of the frontend (`bridge.rs`, `app.rs`, `highlight_worker.rs`) already
// calls them ungated — the host binary exists only so `check`, `clippy`, and
// `nextest` can analyse this crate, and is never run. Gating this module made
// every media type unconstructible in the host build, which is what the
// dead-code lint was correctly reporting.
pub mod media_web;
pub mod session;
pub mod wire;

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

use leptos::prelude::*;
use protocol::{Envelope, FrameKind, StreamPath};

use crate::state::{ActiveAgentRef, AgentInfo, AppState, ConnectionStatus};
use media::{MediaEvent, MediaEventSink, MediaPlatform, MediaStartRequest, RemoteIceCandidate};
use session::{
    EffectiveAudio, VoiceEndReason, VoiceFailure, VoiceGeneration, VoicePhase, VoiceSessionKey,
    VoiceStage, VoiceTarget, VoiceUiState, VoiceUnavailable,
};
use wire::InboundVoice;

/// Protocol ceiling on cumulative ICE candidates for one session
/// (`protocol::MAX_VOICE_ICE_CANDIDATES`). Mirrored here so the client stops
/// sending at the limit rather than tripping a protocol violation, which would
/// wedge the stream.
const MAX_LOCAL_ICE_CANDIDATES: usize = protocol::MAX_VOICE_ICE_CANDIDATES;

/// How long a connected session may stay in ICE `disconnected` before the
/// client tears it down.
///
/// `disconnected` is recoverable, so it must not end a session immediately —
/// but a route that never comes back also never reaches `failed` in some
/// browsers, and the negotiation deadline has already been stood down by then.
/// Without this bound a broken route keeps the microphone open indefinitely;
/// the host's own session lease is not a local capture guarantee.
const DISCONNECT_GRACE_MS: i32 = 15_000;

/// How long a session may sit unconfirmed before the client gives up.
///
/// Bounds the window in which the microphone is open but nothing has proved
/// the host created a session — a `VoiceStart` that is admitted locally and
/// then lost leaves a hot mic with no upper bound otherwise. Deliberately
/// shorter than the server's own 450s lease so the client is the one that
/// notices first and can say why.
const NEGOTIATION_DEADLINE_MS: i32 = 20_000;

/// Live, non-`Send` session state. Exists only while a session is engaged.
///
/// **Runtime presence, not display phase, is the cleanup authority.** A phase
/// can become non-engaged for display reasons; as long as this struct exists,
/// something owns a microphone and must be released.
struct SessionRuntime {
    generation: VoiceGeneration,
    host_id: String,
    session: VoiceSessionKey,
    platform: Rc<dyn MediaPlatform>,
    local_candidates_sent: usize,
    /// Local candidates gathered before `VoiceOffer` was queued. The host
    /// terminates a session that receives a candidate before it has accepted
    /// the offer, so these are held back and flushed in order once the offer
    /// is on the wire.
    pending_local_candidates: Vec<media::LocalIceCandidate>,
    offer_queued: bool,
    ice_complete_pending: bool,
    /// Remote candidates that arrived before `setRemoteDescription` resolved.
    /// Applying one first throws, and the discarded candidate may have been
    /// the only viable direct pair.
    pending_remote_candidates: Vec<RemoteIceCandidate>,
    remote_description_applied: bool,
    /// True while a remote-candidate drain is in flight. `addIceCandidate` is
    /// order-sensitive, so a second batch queues behind the first rather than
    /// racing it in its own spawned future.
    remote_flush_active: bool,
    /// `setTimeout` handle for [`NEGOTIATION_DEADLINE_MS`].
    deadline: Option<i32>,
    /// `setTimeout` handle for [`DISCONNECT_GRACE_MS`], plus the epoch it was
    /// armed under.
    ///
    /// The epoch identifies one outage and is retired by **every** connection
    /// transition — both entering `disconnected` and recovering from it. A
    /// grace expiry only acts when its epoch is still the current one, so a
    /// callback that recovery could not cancel, or that arrives after the
    /// session has dropped and recovered again, cannot tear down a connection
    /// that is fine.
    disconnect_grace: Option<i32>,
    disconnect_epoch: u64,
}

impl SessionRuntime {
    fn clear_deadline(&mut self) {
        clear_timer(&mut self.deadline);
        clear_timer(&mut self.disconnect_grace);
    }
}

fn clear_timer(slot: &mut Option<i32>) {
    if let Some(handle) = slot.take()
        && let Some(window) = browser_window()
    {
        window.clear_timeout_with_handle(handle);
    }
}

/// The browser window, or `None` when this build has no browser.
///
/// Not `web_sys::window()` directly: that reaches an imported static, which
/// **panics** on a non-wasm target rather than returning `None`. The target
/// check therefore has to happen before the call, not on its result. Every
/// native unit test that starts a session arms a deadline and goes through
/// here, so getting this wrong takes the whole suite down.
fn browser_window() -> Option<web_sys::Window> {
    #[cfg(target_arch = "wasm32")]
    {
        web_sys::window()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        None
    }
}

/// Builds the media backend for a session. Production installs the browser
/// one at startup; tests install a fake.
pub type MediaPlatformFactory = Rc<dyn Fn() -> Rc<dyn MediaPlatform>>;

/// One outbound frame. Carries the generation that produced it, so a late
/// failure from a session that has already ended cannot delete or terminate
/// the session that replaced it.
struct OutboundFrame {
    generation: VoiceGeneration,
    host_id: String,
    stream: StreamPath,
    kind: FrameKind,
    payload: serde_json::Value,
}

thread_local! {
    /// Everything the UI renders. Plain data, so an ordinary reactive signal.
    static VOICE_STATE: ArcRwSignal<VoiceUiState> = ArcRwSignal::new(VoiceUiState::default());
    /// Browser handles for the live session.
    static RUNTIME: RefCell<Option<SessionRuntime>> = const { RefCell::new(None) };
    /// Outbound frames, drained strictly in order. Voice signaling is
    /// order-sensitive (`VoiceStart` must be seq 0 and precede `VoiceOffer`),
    /// and independently spawned sends give no ordering guarantee.
    static OUTBOX: RefCell<VecDeque<OutboundFrame>> = const { RefCell::new(VecDeque::new()) };
    static OUTBOX_PUMPING: Cell<bool> = const { Cell::new(false) };
    static SESSION_SEQ: Cell<u64> = const { Cell::new(0) };
    /// Overridable so tests can inject `media::FakeMediaPlatform`.
    static PLATFORM_FACTORY: RefCell<Option<MediaPlatformFactory>> = const { RefCell::new(None) };
}

#[cfg(all(test, not(target_arch = "wasm32")))]
thread_local! {
    /// Frame kinds handed to the transport, in order. Assertable evidence that
    /// signaling order is what the protocol requires.
    ///
    /// Native only: on wasm `pump_outbox` really sends, so there is nothing to
    /// record and nothing that reads this.
    static SENT_LOG: RefCell<Vec<FrameKind>> = const { RefCell::new(Vec::new()) };
}

/// The reactive voice state. Components read this directly; there is exactly
/// one session per client, so there is nothing to key it by.
pub fn ui_state() -> ArcRwSignal<VoiceUiState> {
    VOICE_STATE.with(Clone::clone)
}

fn update_state(f: impl FnOnce(&mut VoiceUiState) -> bool) {
    VOICE_STATE.with(|signal| {
        signal.update(|state| {
            f(state);
        });
    });
}

fn read_state<T>(f: impl FnOnce(&VoiceUiState) -> T) -> T {
    VOICE_STATE.with(|signal| signal.with_untracked(f))
}

// ── Session identity ────────────────────────────────────────────────────────

/// Fill `buffer` with cryptographically random bytes.
///
/// The session id is the capability that names the `/voice/<id>` stream, so it
/// comes from `crypto.getRandomValues`, not `Math.random`. If Web Crypto is
/// missing the call fails and voice refuses to start — there is no weaker
/// source to fall back to.
#[cfg(target_arch = "wasm32")]
fn random_bytes(buffer: &mut [u8]) -> Result<(), String> {
    let window = web_sys::window().ok_or("no browser window")?;
    let crypto = window
        .crypto()
        .map_err(|_| "this build has no Web Crypto API, so a voice session id cannot be minted")?;
    crypto
        .get_random_values_with_u8_array(buffer)
        .map_err(|_| "the browser refused to generate a voice session id")?;
    Ok(())
}

/// Off-wasm this runs only in unit tests, which have no Web Crypto. A counter
/// keeps ids distinct within one test process. Never compiled into the wasm
/// bundle, so it cannot weaken the shipped id.
#[cfg(not(target_arch = "wasm32"))]
fn random_bytes(buffer: &mut [u8]) -> Result<(), String> {
    let seed = SESSION_SEQ.with(|counter| {
        let next = counter.get().wrapping_add(1);
        counter.set(next);
        next
    });
    for (index, byte) in buffer.iter_mut().enumerate() {
        *byte = seed.wrapping_mul(31).wrapping_add(index as u64) as u8;
    }
    Ok(())
}

/// Mint a UUIDv4 session id.
///
/// The protocol requires it: `dev-docs/02-protocol.md` §3 states the last
/// stream segment is always a UUIDv4, and the server parses it with
/// `Uuid::parse_str` (`server/src/router.rs::parse_voice_session_id`,
/// `server/src/connection.rs::voice_session_id`). Anything else is rejected —
/// and rejected on the *host* stream, where this controller would never see
/// it, so a non-UUID id fails silently and hangs the session.
///
/// `frontend` has no `uuid` dependency and adding one would touch the root
/// lockfile, which this domain does not own, so the 16 bytes are formatted
/// here directly.
fn new_session_id() -> Result<String, String> {
    let mut bytes = [0_u8; 16];
    random_bytes(&mut bytes)?;
    // RFC 4122 §4.4: version 4 in the high nibble of octet 6, variant 10 in
    // the two high bits of octet 8.
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    ))
}

/// Install a media platform factory. Production installs the browser one at
/// startup; tests install a fake.
pub fn set_platform_factory(factory: MediaPlatformFactory) {
    PLATFORM_FACTORY.with(|slot| *slot.borrow_mut() = Some(factory));
}

fn make_platform() -> Option<Rc<dyn MediaPlatform>> {
    PLATFORM_FACTORY.with(|slot| slot.borrow().as_ref().map(|factory| factory()))
}

// ── Availability ────────────────────────────────────────────────────────────

/// Can this chat start a voice session right now?
///
/// Every rejection carries a sentence the UI shows verbatim. Availability for
/// the host itself is **server-declared** (`HostSettings.voice.availability`) —
/// the client never infers it from the presence of a credential or a setting.
pub fn availability(
    state: &AppState,
    agent: Option<&ActiveAgentRef>,
) -> Result<VoiceTarget, VoiceUnavailable> {
    let Some(agent) = agent else {
        return Err(VoiceUnavailable::NoAgent);
    };
    if !matches!(
        state.connection_status_for_host_untracked(&agent.host_id),
        ConnectionStatus::Connected
    ) {
        return Err(VoiceUnavailable::HostDisconnected);
    }
    match state
        .host_settings_untracked(&agent.host_id)
        .map(|settings| settings.voice.availability)
    {
        Some(protocol::VoiceAvailability::Available { .. }) => {}
        Some(protocol::VoiceAvailability::Unavailable { reason }) => {
            return Err(VoiceUnavailable::HostReported(
                unavailable_reason_text(reason).to_owned(),
            ));
        }
        // Settings not received yet: the host has not said voice is available,
        // so it is not offered. Absence is not consent.
        None => {
            return Err(VoiceUnavailable::HostReported(
                "Waiting for this host to report whether voice is available".to_owned(),
            ));
        }
    }

    let info = find_agent(state, agent).ok_or(VoiceUnavailable::NoAgent)?;
    if info.fatal_error.is_some() {
        return Err(VoiceUnavailable::AgentTerminated);
    }
    if !info.started {
        return Err(VoiceUnavailable::AgentNotStarted);
    }

    let target = VoiceTarget::new(agent.clone(), info.instance_stream.clone());
    let busy = read_state(|voice| voice.is_engaged() && !voice.is_bound_to(&target));
    if busy {
        return Err(VoiceUnavailable::BusyElsewhere);
    }
    Ok(target)
}

fn unavailable_reason_text(reason: protocol::VoiceUnavailableReason) -> &'static str {
    match reason {
        protocol::VoiceUnavailableReason::NotEnabled => "Voice is turned off for this host",
        protocol::VoiceUnavailableReason::RegionNotConfigured => {
            "This host has no AWS region configured for voice"
        }
        protocol::VoiceUnavailableReason::ServerAdapterUnavailable => {
            "This host cannot reach the voice provider"
        }
        protocol::VoiceUnavailableReason::NoReachableCandidate => {
            "There is no direct network route to this host for audio"
        }
    }
}

fn find_agent(state: &AppState, agent: &ActiveAgentRef) -> Option<AgentInfo> {
    state.agents.with_untracked(|agents| {
        agents
            .iter()
            .find(|info| info.host_id == agent.host_id && info.agent_id == agent.agent_id)
            .cloned()
    })
}

// ── Lifecycle ───────────────────────────────────────────────────────────────

/// Release any live runtime, whatever the display phase says.
///
/// Ownership is tracked by the presence of `RUNTIME`, never by the phase. A
/// session whose phase has moved on for display reasons can still own a
/// microphone, and this is the one place that guarantees it is released.
fn release_runtime() -> Option<SessionRuntime> {
    let runtime = RUNTIME.with(|slot| slot.borrow_mut().take());
    if let Some(mut runtime) = runtime {
        runtime.clear_deadline();
        runtime.platform.stop();
        return Some(runtime);
    }
    None
}

/// Start a session for `target`.
///
/// The caller reveals the chat first so `target` is the derived composer owner
/// — see `components::voice_layer`. `target` must have come from
/// [`availability`], which checks the host's server-declared availability and
/// the agent's liveness.
pub fn start(target: VoiceTarget) {
    if read_state(VoiceUiState::is_engaged) {
        return;
    }
    // A previous session may have left a runtime behind (a non-fatal error
    // moved the phase without teardown, say). Release it before replacing it,
    // so a start can never orphan a live microphone.
    release_runtime();

    let Some(platform) = make_platform() else {
        // No audio backend at all. Fail loudly rather than leaving a control
        // that looks armed and does nothing.
        fail_before_start(VoiceFailure {
            stage: VoiceStage::Media,
            message: "Voice is not available in this build: no audio backend is installed."
                .to_owned(),
            retryable: false,
        });
        return;
    };

    let session_id = match new_session_id() {
        Ok(session_id) => session_id,
        Err(error) => {
            fail_before_start(VoiceFailure {
                stage: VoiceStage::Negotiation,
                message: error,
                retryable: false,
            });
            return;
        }
    };
    let session = VoiceSessionKey {
        stream: wire::voice_stream(&session_id),
        session_id,
    };

    let mut generation = VoiceGeneration::default();
    update_state(|voice| {
        generation = voice.begin(target.clone(), session.clone());
        true
    });

    RUNTIME.with(|slot| {
        *slot.borrow_mut() = Some(SessionRuntime {
            generation,
            host_id: target.host_id().to_owned(),
            session: session.clone(),
            platform: platform.clone(),
            local_candidates_sent: 0,
            pending_local_candidates: Vec::new(),
            offer_queued: false,
            ice_complete_pending: false,
            pending_remote_candidates: Vec::new(),
            remote_description_applied: false,
            remote_flush_active: false,
            deadline: None,
            disconnect_grace: None,
            disconnect_epoch: 0,
        });
    });
    arm_negotiation_deadline(generation);

    // `VoiceStart` is seq 0 on the new stream and opens the session before the
    // microphone prompt, so a host that will refuse (voice disabled, agent
    // gone, another session live) does so before the user is asked for the
    // microphone.
    enqueue(
        generation,
        target.host_id(),
        session.stream.clone(),
        FrameKind::VoiceStart,
        &wire::start_payload(&session, &target),
    );

    let events: MediaEventSink = Rc::new(move |event| on_media_event(generation, event));
    let start_future = platform.start(
        MediaStartRequest {
            ice_servers: Vec::new(),
        },
        events,
    );
    let session_for_offer = session.clone();
    let platform_for_stale = platform.clone();
    spawn(async move {
        match start_future.await {
            Ok(started) => {
                if !apply_mic_grant(generation, started.effective_audio) {
                    // The session ended while acquisition was in flight. The
                    // platform's own cancellation check normally catches this,
                    // but stop it explicitly rather than trusting the race:
                    // a live microphone with no owner is the worst outcome
                    // here, and `stop` is idempotent.
                    platform_for_stale.stop();
                    return;
                }
                let host_id = match host_for(generation) {
                    Some(host_id) => host_id,
                    None => {
                        platform_for_stale.stop();
                        return;
                    }
                };
                enqueue_owned(OutboundFrame {
                    generation,
                    host_id,
                    stream: session_for_offer.stream.clone(),
                    kind: FrameKind::VoiceOffer,
                    payload: to_value(&wire::offer_payload(&session_for_offer, started.offer_sdp)),
                });
                // The offer is queued ahead of every candidate in the same
                // FIFO outbox, so flushing now cannot reorder them on the wire.
                mark_offer_queued(generation);
            }
            Err(error) => {
                if error.is_cancellation() {
                    // Our own teardown interrupted acquisition. Nothing is
                    // held and nothing went wrong.
                    return;
                }
                if !is_current(generation) {
                    platform_for_stale.stop();
                    return;
                }
                let reason = if error.stage == VoiceStage::Microphone {
                    VoiceEndReason::PermissionDenied
                } else {
                    VoiceEndReason::MediaFailed
                };
                end_with_failure(
                    reason,
                    VoiceFailure {
                        stage: error.stage,
                        message: error.message,
                        retryable: error.retryable,
                    },
                );
            }
        }
    });
}

/// Report a refusal that happened before a session existed.
fn fail_before_start(failure: VoiceFailure) {
    crate::components::header::report_user_error(format!(
        "Voice — {}: {}",
        failure.stage.label(),
        failure.message
    ));
    update_state(|voice| voice.fail_to_start(failure.clone()));
}

fn apply_mic_grant(generation: VoiceGeneration, audio: EffectiveAudio) -> bool {
    let mut applied = false;
    update_state(|voice| {
        applied = voice.mic_granted(generation, audio);
        applied
    });
    applied
}

fn host_for(generation: VoiceGeneration) -> Option<String> {
    RUNTIME.with(|slot| {
        slot.borrow()
            .as_ref()
            .filter(|runtime| runtime.generation == generation)
            .map(|runtime| runtime.host_id.clone())
    })
}

fn is_current(generation: VoiceGeneration) -> bool {
    read_state(|voice| voice.generation == generation)
}

// ── Negotiation deadline ────────────────────────────────────────────────────

/// Bound the window in which the microphone is open but unconfirmed.
///
/// `send_frame` reports local admission only; a frame can be admitted and then
/// lost, in which case the host never creates a session and none of its own
/// timers ever arm. Without this the strip sits on "Connecting" with a live
/// microphone indefinitely.
/// Schedule `on_fire` and hand the timer handle to `store`.
///
/// The closure is leaked rather than retained: every timer here is one-shot
/// and every handler is generation-guarded, so a callback that fires after
/// teardown is already a no-op.
///
/// Off-wasm [`browser_window`] is `None`, so nothing is scheduled and the
/// function returns without a timer — the honest outcome for a build with no
/// event loop, and it keeps the handlers reachable rather than stranding them
/// behind a stub.
fn arm_timer(
    generation: VoiceGeneration,
    delay_ms: i32,
    on_fire: impl FnMut() + 'static,
    store: impl FnOnce(&mut SessionRuntime, i32),
) {
    use wasm_bindgen::JsCast as _;
    use wasm_bindgen::prelude::Closure;

    let Some(window) = browser_window() else {
        return;
    };
    let callback = Closure::<dyn FnMut()>::new(on_fire);
    let handle = window.set_timeout_with_callback_and_timeout_and_arguments_0(
        callback.as_ref().unchecked_ref(),
        delay_ms,
    );
    callback.forget();
    if let Ok(handle) = handle {
        RUNTIME.with(|slot| {
            if let Some(runtime) = slot
                .borrow_mut()
                .as_mut()
                .filter(|runtime| runtime.generation == generation)
            {
                store(runtime, handle);
            }
        });
    }
}

fn arm_negotiation_deadline(generation: VoiceGeneration) {
    arm_timer(
        generation,
        NEGOTIATION_DEADLINE_MS,
        move || on_negotiation_deadline(generation),
        |runtime, handle| runtime.deadline = Some(handle),
    );
}

/// Bound how long a *connected* session may stay in ICE `disconnected`.
fn arm_disconnect_grace(generation: VoiceGeneration, epoch: u64) {
    arm_timer(
        generation,
        DISCONNECT_GRACE_MS,
        move || on_disconnect_grace_expired(generation, epoch),
        |runtime, handle| runtime.disconnect_grace = Some(handle),
    );
}

fn on_negotiation_deadline(generation: VoiceGeneration) {
    if !is_current(generation) || !read_state(VoiceUiState::is_engaged) {
        return;
    }
    if read_state(|voice| matches!(voice.phase, VoicePhase::Live)) {
        return;
    }
    let admitted = read_state(|voice| voice.host_admitted);
    let message = if admitted {
        "The host accepted voice but audio never connected. Check that this \
         machine and the host share a direct network route."
            .to_owned()
    } else {
        "The host did not answer the voice request. It may not have received \
         it, or voice may be unavailable there."
            .to_owned()
    };
    end_with_failure(
        VoiceEndReason::TransportFailed,
        VoiceFailure {
            stage: VoiceStage::Negotiation,
            message,
            retryable: true,
        },
    );
}

/// The grace period expired with the connection still down.
///
/// Guarded by both generation and epoch: a session that dropped, recovered,
/// and dropped again has a newer epoch, so the older timer is inert.
fn on_disconnect_grace_expired(generation: VoiceGeneration, epoch: u64) {
    let still_waiting = RUNTIME.with(|slot| {
        slot.borrow().as_ref().is_some_and(|runtime| {
            runtime.generation == generation && runtime.disconnect_epoch == epoch
        })
    });
    if !still_waiting || !is_current(generation) || !read_state(VoiceUiState::is_engaged) {
        return;
    }
    end_with_failure(
        VoiceEndReason::TransportFailed,
        VoiceFailure {
            stage: VoiceStage::Media,
            message: "The audio connection dropped and did not recover.".to_owned(),
            retryable: true,
        },
    );
}

// ── Teardown ────────────────────────────────────────────────────────────────

/// End the session. Safe to call at any time and from any path.
///
/// Local media is stopped **synchronously**, before the stop frame is queued.
/// "Switching agents ends voice immediately" is a claim about the microphone,
/// not about a round trip.
///
/// Media is released whenever a runtime exists, even if the reducer declines
/// the transition — ownership follows the runtime, never the phase.
pub fn end(reason: VoiceEndReason) {
    let mut ended = None;
    update_state(|voice| {
        ended = voice.end(reason);
        ended.is_some()
    });

    let Some(runtime) = release_runtime() else {
        return;
    };
    // Drop this session's queued frames; another session's must survive.
    OUTBOX.with(|outbox| {
        outbox
            .borrow_mut()
            .retain(|frame| frame.generation != runtime.generation);
    });
    if let Some(session) = ended.and_then(|ended| ended.session) {
        enqueue(
            runtime.generation,
            &runtime.host_id,
            session.stream.clone(),
            FrameKind::VoiceStop,
            &wire::stop_payload(&session, reason),
        );
    }
}

fn end_with_failure(reason: VoiceEndReason, failure: VoiceFailure) {
    // The generation is read before teardown and `end` leaves it alone, so the
    // failure lands on the session it actually describes.
    let generation = read_state(|voice| voice.generation);
    end(reason);
    report_failure(failure, generation);
}

/// Surface a terminal failure both in the strip and as a global user-facing
/// error. Only called after teardown.
fn report_failure(failure: VoiceFailure, generation: VoiceGeneration) {
    crate::components::header::report_user_error(format!(
        "Voice — {}: {}",
        failure.stage.label(),
        failure.message
    ));
    update_state(|voice| voice.fail(generation, failure.clone()));
}

/// Surface a problem the session survived.
///
/// Never changes the phase, so End stays available and the runtime keeps
/// owning its media. Using the terminal path here would hide the End control
/// while the microphone was still open.
fn report_warning(failure: VoiceFailure, generation: VoiceGeneration) {
    log::warn!(
        "voice warning ({}): {}",
        failure.stage.label(),
        failure.message
    );
    update_state(|voice| voice.warn(generation, failure.clone()));
}

pub fn toggle_mute() {
    let entry = RUNTIME.with(|slot| {
        slot.borrow()
            .as_ref()
            .map(|runtime| (runtime.generation, runtime.platform.clone()))
    });
    let Some((generation, platform)) = entry else {
        return;
    };
    let next = !read_state(|voice| voice.muted);
    match platform.set_muted(next) {
        Ok(()) => update_state(|voice| {
            voice.clear_warning(generation);
            voice.set_muted(generation, next)
        }),
        // A refused mute is a warning, not a teardown: the session is still
        // live and the user still needs the End control.
        Err(error) => report_warning(
            VoiceFailure {
                stage: error.stage,
                message: error.message,
                retryable: error.retryable,
            },
            generation,
        ),
    }
}

/// Silence remote output immediately while the microphone stays live.
///
/// This is a **local** action: it pauses the audio element and sends nothing to
/// the host. It is deliberately not called "interrupt" — whether the agent
/// stops talking depends on provider-side barge-in reacting to the still-live
/// microphone, which is unverified live behaviour and not something this
/// control can claim.
///
/// Playback is marked blocked so the existing "Tap to hear" control reappears,
/// and the controller re-plays automatically when the agent starts a new
/// utterance — without that, a silenced session stays silent for the rest of
/// its life because `ontrack` fires only once.
pub fn silence_output() {
    let entry = RUNTIME.with(|slot| {
        slot.borrow()
            .as_ref()
            .map(|runtime| (runtime.generation, runtime.platform.clone()))
    });
    let Some((generation, platform)) = entry else {
        return;
    };
    match platform.silence_output() {
        Ok(()) => update_state(|voice| voice.set_playback_blocked(generation, true)),
        Err(error) => report_warning(
            VoiceFailure {
                stage: error.stage,
                message: error.message,
                retryable: error.retryable,
            },
            generation,
        ),
    }
}

/// Retry blocked output — after a user gesture, or automatically when the
/// agent starts speaking again following an interruption.
pub fn resume_playback() {
    let entry = RUNTIME.with(|slot| {
        slot.borrow()
            .as_ref()
            .map(|runtime| (runtime.generation, runtime.platform.clone()))
    });
    let Some((generation, platform)) = entry else {
        return;
    };
    let future = platform.resume_playback();
    spawn(async move {
        match future.await {
            Ok(()) => update_state(|voice| voice.set_playback_blocked(generation, false)),
            Err(error) => report_warning(
                VoiceFailure {
                    stage: error.stage,
                    message: error.message,
                    retryable: error.retryable,
                },
                generation,
            ),
        }
    });
}

/// Clear the residual ended/failed banner.
pub fn dismiss() {
    update_state(VoiceUiState::dismiss);
}

// ── Media events ────────────────────────────────────────────────────────────

fn on_media_event(generation: VoiceGeneration, event: MediaEvent) {
    // Two guards, not one. The generation rejects events from a superseded
    // session; the phase rejects events from *this* session that arrive after
    // teardown has begun — notably the `closed`/`ended` callbacks the browser
    // fires in response to our own `stop`, which must not be reported as a
    // connection failure the user needs to know about.
    if !is_current(generation) || !read_state(VoiceUiState::is_engaged) {
        return;
    }
    match event {
        MediaEvent::LocalIceCandidate(candidate) => queue_local_candidate(generation, candidate),
        MediaEvent::LocalIceComplete => mark_local_ice_complete(generation),
        MediaEvent::RemoteTrackAttached => {
            update_state(|voice| voice.set_playback_blocked(generation, false));
        }
        MediaEvent::PlaybackBlocked(_) => {
            update_state(|voice| voice.set_playback_blocked(generation, true));
        }
        MediaEvent::Connected => {
            // Recovery: stand the grace timer down before it can fire, and
            // retire the epoch it was armed under.
            //
            // Cancelling the handle is not enough on its own. `clearTimeout`
            // cannot recall a callback the browser has already dispatched, and
            // off-wasm no timer is armed at all — so a grace expiry can still
            // arrive after recovery. Retiring the epoch is what makes that
            // arrival inert; the handle is just an optimisation that stops
            // most of them from happening.
            RUNTIME.with(|slot| {
                if let Some(runtime) = slot
                    .borrow_mut()
                    .as_mut()
                    .filter(|runtime| runtime.generation == generation)
                {
                    runtime.disconnect_epoch = runtime.disconnect_epoch.wrapping_add(1);
                    clear_timer(&mut runtime.disconnect_grace);
                }
            });
            update_state(|voice| {
                voice.clear_warning(generation);
                voice.connected(generation)
            });
        }
        MediaEvent::ConnectionUnstable => {
            // Recoverable, so it must not end the session outright — ICE
            // routinely returns to `connected` after a roam or a
            // consent-freshness lapse. But a route that never comes back may
            // also never reach `failed`, and the negotiation deadline has
            // already stood down by now, so the microphone needs its own
            // upper bound. Warn now; tear down if recovery does not arrive.
            let epoch = RUNTIME.with(|slot| {
                slot.borrow_mut()
                    .as_mut()
                    .filter(|runtime| runtime.generation == generation)
                    .map(|runtime| {
                        runtime.disconnect_epoch = runtime.disconnect_epoch.wrapping_add(1);
                        clear_timer(&mut runtime.disconnect_grace);
                        runtime.disconnect_epoch
                    })
            });
            let Some(epoch) = epoch else {
                return;
            };
            report_warning(
                VoiceFailure {
                    stage: VoiceStage::Media,
                    message: "The audio connection is unstable and is trying to recover."
                        .to_owned(),
                    retryable: true,
                },
                generation,
            );
            arm_disconnect_grace(generation, epoch);
        }
        MediaEvent::Disconnected(detail) => {
            end_with_failure(
                VoiceEndReason::TransportFailed,
                VoiceFailure {
                    stage: VoiceStage::Media,
                    message: format!("The audio connection {detail}."),
                    retryable: true,
                },
            );
        }
        MediaEvent::MicrophoneEnded => {
            end_with_failure(
                VoiceEndReason::MediaFailed,
                VoiceFailure {
                    stage: VoiceStage::Microphone,
                    message: "The microphone stopped. It may have been unplugged or taken by \
                              another app."
                        .to_owned(),
                    retryable: true,
                },
            );
        }
    }
}

// ── Local ICE ordering ──────────────────────────────────────────────────────

/// Hold a local candidate until the offer is queued, then send in order.
fn queue_local_candidate(generation: VoiceGeneration, candidate: media::LocalIceCandidate) {
    let ready = RUNTIME.with(|slot| {
        let mut borrow = slot.borrow_mut();
        let Some(runtime) = borrow
            .as_mut()
            .filter(|runtime| runtime.generation == generation)
        else {
            return false;
        };
        if !runtime.offer_queued {
            runtime.pending_local_candidates.push(candidate.clone());
            return false;
        }
        true
    });
    if ready {
        send_local_candidate(generation, candidate);
    }
}

fn send_local_candidate(generation: VoiceGeneration, candidate: media::LocalIceCandidate) {
    let entry = RUNTIME.with(|slot| {
        let mut borrow = slot.borrow_mut();
        let runtime = borrow
            .as_mut()
            .filter(|runtime| runtime.generation == generation)?;
        if runtime.local_candidates_sent >= MAX_LOCAL_ICE_CANDIDATES {
            // The protocol caps cumulative candidates per session. Stopping
            // here keeps the stream valid; sending anyway is a protocol
            // violation, which wedges it.
            log::warn!(
                "voice: reached the protocol cap of {MAX_LOCAL_ICE_CANDIDATES} ICE candidates; \
                 later candidates are not offered and connectivity may be reduced"
            );
            return None;
        }
        runtime.local_candidates_sent += 1;
        Some((runtime.host_id.clone(), runtime.session.clone()))
    });
    let Some((host_id, session)) = entry else {
        return;
    };
    enqueue(
        generation,
        &host_id,
        session.stream.clone(),
        FrameKind::VoiceIceCandidate,
        &wire::ice_payload(&session, vec![candidate]),
    );
}

fn mark_local_ice_complete(generation: VoiceGeneration) {
    let flushed = RUNTIME.with(|slot| {
        let mut borrow = slot.borrow_mut();
        let Some(runtime) = borrow
            .as_mut()
            .filter(|runtime| runtime.generation == generation)
        else {
            return false;
        };
        if !runtime.offer_queued {
            runtime.ice_complete_pending = true;
            return false;
        }
        true
    });
    if flushed {
        send_ice_complete(generation);
    }
}

fn send_ice_complete(generation: VoiceGeneration) {
    if let Some((host_id, session)) = runtime_addr(generation) {
        enqueue(
            generation,
            &host_id,
            session.stream.clone(),
            FrameKind::VoiceIceCandidatesComplete,
            &wire::ice_complete_payload(&session),
        );
    }
}

/// The offer is in the ordered outbox. Drain everything gathered before it.
fn mark_offer_queued(generation: VoiceGeneration) {
    let (pending, complete) = RUNTIME.with(|slot| {
        let mut borrow = slot.borrow_mut();
        let Some(runtime) = borrow
            .as_mut()
            .filter(|runtime| runtime.generation == generation)
        else {
            return (Vec::new(), false);
        };
        runtime.offer_queued = true;
        let complete = std::mem::take(&mut runtime.ice_complete_pending);
        (
            std::mem::take(&mut runtime.pending_local_candidates),
            complete,
        )
    });
    for candidate in pending {
        send_local_candidate(generation, candidate);
    }
    if complete {
        send_ice_complete(generation);
    }
}

fn runtime_addr(generation: VoiceGeneration) -> Option<(String, VoiceSessionKey)> {
    RUNTIME.with(|slot| {
        slot.borrow()
            .as_ref()
            .filter(|runtime| runtime.generation == generation)
            .map(|runtime| (runtime.host_id.clone(), runtime.session.clone()))
    })
}

// ── Inbound protocol ────────────────────────────────────────────────────────

/// Handle an inbound `/voice/<id>` envelope. Returns `true` when the frame
/// belonged to voice — the dispatcher uses that to decide whether it handled
/// the envelope.
pub fn handle_inbound(host_id: &str, envelope: &Envelope) -> bool {
    let Some(inbound_session_id) = wire::session_id_from_stream(&envelope.stream) else {
        return false;
    };

    // A frame for a session we do not own — a late frame for a session that
    // already ended, or one for a different host — is dropped, never applied.
    let owned = RUNTIME.with(|slot| {
        slot.borrow().as_ref().is_some_and(|runtime| {
            runtime.host_id == host_id && runtime.session.session_id == inbound_session_id
        })
    });
    if !owned {
        log::debug!(
            "voice: ignoring {} for unowned session on {}",
            envelope.kind,
            envelope.stream
        );
        return true;
    }

    // The payload's own session id must agree with the stream it arrived on.
    if let Some(claimed) = wire::payload_session_id(envelope.kind, envelope)
        && claimed.0 != inbound_session_id
    {
        log::error!(
            "voice: {} claims session {} on stream {}",
            envelope.kind,
            claimed.0,
            envelope.stream
        );
        end_with_failure(
            VoiceEndReason::ServerEnded,
            VoiceFailure {
                stage: VoiceStage::Signaling,
                message: "The host sent a voice frame for a different session.".to_owned(),
                retryable: false,
            },
        );
        return true;
    }

    let generation = read_state(|voice| voice.generation);
    match wire::decode(envelope.kind, envelope) {
        Ok(Some(inbound)) => apply_inbound(generation, inbound),
        Ok(None) => {}
        Err(error) => {
            log::error!("voice: {error}");
            end_with_failure(
                VoiceEndReason::ServerEnded,
                VoiceFailure {
                    stage: VoiceStage::Signaling,
                    message: error,
                    retryable: false,
                },
            );
        }
    }
    true
}

/// A `CommandError` the host raised for one of our voice frames.
///
/// Voice admission failures do not always come back as a typed `VoiceError` on
/// the voice stream: a malformed or unparseable stream path is rejected before
/// the host has a voice session to answer on, and surfaces as a `CommandError`
/// on the **host** stream instead. Without this correlation the session simply
/// hangs until its own deadline with nothing to show the user.
pub fn handle_command_error(
    host_id: &str,
    request_stream: &StreamPath,
    request_kind: FrameKind,
    message: &str,
) -> bool {
    if !matches!(
        request_kind,
        FrameKind::VoiceStart
            | FrameKind::VoiceOffer
            | FrameKind::VoiceIceCandidate
            | FrameKind::VoiceIceCandidatesComplete
            | FrameKind::VoiceStop
    ) {
        return false;
    }
    // Correlate on the exact voice stream, not just the host. A late rejection
    // of a *previous* session's frame arrives on that session's stream; acting
    // on it would terminate whatever session happens to be live now.
    let ours = RUNTIME.with(|slot| {
        slot.borrow().as_ref().is_some_and(|runtime| {
            runtime.host_id == host_id && runtime.session.stream == *request_stream
        })
    });
    if !ours {
        log::debug!(
            "voice: ignoring {request_kind} rejection on {request_stream}, which is not this \
             session's stream"
        );
        return false;
    }
    // A rejected Stop needs no action — we are already tearing down.
    if matches!(request_kind, FrameKind::VoiceStop) {
        log::warn!("voice: host rejected voice_stop: {message}");
        return true;
    }
    end_with_failure(
        VoiceEndReason::ServerEnded,
        VoiceFailure {
            stage: VoiceStage::Signaling,
            message: format!("The host rejected {request_kind}: {message}"),
            retryable: false,
        },
    );
    true
}

fn apply_inbound(generation: VoiceGeneration, inbound: InboundVoice) {
    match inbound {
        InboundVoice::Ready {
            target,
            direct_connections_only,
            expires_after_seconds,
        } => {
            // The host echoes the immutable target back. A Ready for anything
            // else means the two sides disagree about what this session is
            // bound to, which is exactly the confusion the immutable binding
            // exists to prevent.
            let bound = read_state(|voice| voice.target.clone());
            let matches_target = bound.as_ref().is_some_and(|bound| {
                bound.agent.agent_id == target.agent_id
                    && bound.instance_stream == target.instance_stream
            });
            if !matches_target {
                end_with_failure(
                    VoiceEndReason::ServerEnded,
                    VoiceFailure {
                        stage: VoiceStage::Signaling,
                        message: "The host accepted voice for a different agent.".to_owned(),
                        retryable: false,
                    },
                );
                return;
            }
            if expires_after_seconds == 0 {
                end_with_failure(
                    VoiceEndReason::ServerEnded,
                    VoiceFailure {
                        stage: VoiceStage::Signaling,
                        message: "The host accepted voice with no session lease.".to_owned(),
                        retryable: false,
                    },
                );
                return;
            }
            update_state(|voice| voice.ready(generation, direct_connections_only));
        }
        InboundVoice::Answer { sdp } => apply_answer(generation, sdp),
        InboundVoice::RemoteCandidates(candidates) => {
            queue_remote_candidates(generation, candidates);
        }
        InboundVoice::RemoteCandidatesComplete => {}
        InboundVoice::State {
            activity,
            progress,
            caption,
            transcript,
            tool_notice,
            ended,
        } => {
            if let Some(activity) = activity {
                let was_speaking = read_state(|voice| {
                    matches!(voice.activity, session::VoiceActivity::AgentSpeaking)
                });
                update_state(|voice| voice.set_activity(generation, activity));
                // A new utterance after a barge-in: restore output, otherwise
                // the rest of the session is silent.
                if !was_speaking
                    && matches!(activity, session::VoiceActivity::AgentSpeaking)
                    && read_state(|voice| voice.playback_blocked)
                {
                    resume_playback();
                }
            }
            if let Some(line) = progress {
                update_state(|voice| voice.push_progress(generation, line.clone()));
            }
            // Caption and transcript are only ever *set* from a frame that
            // carries them. A `VoiceState` without them means the host had
            // nothing new to say, not that the last thing said should vanish.
            update_state(|voice| voice.set_caption(generation, caption.clone()));
            if let Some(line) = transcript {
                update_state(|voice| voice.push_transcript(generation, line.clone()));
            }
            update_state(|voice| voice.set_tool_notice(generation, tool_notice.clone()));
            if let Some(reason) = ended {
                end(reason);
            }
        }
        InboundVoice::Error { failure, fatal } => {
            if fatal {
                end_with_failure(VoiceEndReason::ServerEnded, failure);
            } else {
                // Non-fatal by the protocol's own definition: the session
                // continues. Show it without tearing anything down, and
                // without hiding the End control.
                report_warning(failure, generation);
            }
        }
    }
}

fn apply_answer(generation: VoiceGeneration, sdp: String) {
    let platform = RUNTIME.with(|slot| {
        slot.borrow()
            .as_ref()
            .filter(|runtime| runtime.generation == generation)
            .map(|runtime| runtime.platform.clone())
    });
    let Some(platform) = platform else {
        return;
    };
    let future = platform.accept_answer(sdp);
    spawn(async move {
        match future.await {
            Ok(()) => flush_remote_candidates(generation),
            Err(error) => {
                if error.is_cancellation() || !is_current(generation) {
                    return;
                }
                end_with_failure(
                    VoiceEndReason::MediaFailed,
                    VoiceFailure {
                        stage: error.stage,
                        message: error.message,
                        retryable: error.retryable,
                    },
                );
            }
        }
    });
}

/// Hold remote candidates until the answer has actually been applied, then
/// drain them through a single per-session pump.
///
/// Two separate constraints, both real:
///
/// - `addIceCandidate` before `setRemoteDescription` resolves throws, and the
///   rejected candidate is gone — it may have been the only viable direct pair.
/// - `addIceCandidate` is order-sensitive, so a second batch must not race the
///   first. Everything goes through `pending_remote_candidates` and exactly one
///   drain runs at a time, rather than each batch spawning its own future.
fn queue_remote_candidates(generation: VoiceGeneration, candidates: Vec<RemoteIceCandidate>) {
    let should_pump = RUNTIME.with(|slot| {
        let mut borrow = slot.borrow_mut();
        let Some(runtime) = borrow
            .as_mut()
            .filter(|runtime| runtime.generation == generation)
        else {
            return false;
        };
        runtime.pending_remote_candidates.extend(candidates);
        runtime.remote_description_applied && !runtime.remote_flush_active
    });
    if should_pump {
        pump_remote_candidates(generation);
    }
}

/// The answer has been applied; release anything that was waiting on it.
fn flush_remote_candidates(generation: VoiceGeneration) {
    let should_pump = RUNTIME.with(|slot| {
        let mut borrow = slot.borrow_mut();
        let Some(runtime) = borrow
            .as_mut()
            .filter(|runtime| runtime.generation == generation)
        else {
            return false;
        };
        runtime.remote_description_applied = true;
        !runtime.remote_flush_active
    });
    if should_pump {
        pump_remote_candidates(generation);
    }
}

/// Drain the queue one candidate at a time. Re-checks the queue after each
/// application so candidates that arrive mid-drain are picked up in order
/// rather than starting a competing pump.
fn pump_remote_candidates(generation: VoiceGeneration) {
    let entry = RUNTIME.with(|slot| {
        let mut borrow = slot.borrow_mut();
        let runtime = borrow
            .as_mut()
            .filter(|runtime| runtime.generation == generation)?;
        if runtime.remote_flush_active || !runtime.remote_description_applied {
            return None;
        }
        runtime.remote_flush_active = true;
        Some(runtime.platform.clone())
    });
    let Some(platform) = entry else {
        return;
    };

    spawn(async move {
        loop {
            if !is_current(generation) {
                break;
            }
            let next = RUNTIME.with(|slot| {
                slot.borrow_mut()
                    .as_mut()
                    .filter(|runtime| runtime.generation == generation)
                    .and_then(|runtime| {
                        (!runtime.pending_remote_candidates.is_empty())
                            .then(|| runtime.pending_remote_candidates.remove(0))
                    })
            });
            let Some(candidate) = next else {
                break;
            };
            if let Err(error) = platform.add_remote_candidate(candidate).await {
                // One rejected candidate degrades connectivity but is not on
                // its own fatal; ICE may still converge on another pair.
                log::warn!("voice: {}", error.message);
            }
        }
        RUNTIME.with(|slot| {
            if let Some(runtime) = slot
                .borrow_mut()
                .as_mut()
                .filter(|runtime| runtime.generation == generation)
            {
                runtime.remote_flush_active = false;
            }
        });
    });
}

// ── Outbound queue ──────────────────────────────────────────────────────────

fn to_value<T: serde::Serialize>(payload: &T) -> serde_json::Value {
    serde_json::to_value(payload).unwrap_or(serde_json::Value::Null)
}

fn enqueue<T: serde::Serialize>(
    generation: VoiceGeneration,
    host_id: &str,
    stream: StreamPath,
    kind: FrameKind,
    payload: &T,
) {
    enqueue_owned(OutboundFrame {
        generation,
        host_id: host_id.to_owned(),
        stream,
        kind,
        payload: to_value(payload),
    });
}

fn enqueue_owned(frame: OutboundFrame) {
    OUTBOX.with(|outbox| outbox.borrow_mut().push_back(frame));
    pump_outbox();
}

fn pump_outbox() {
    if OUTBOX_PUMPING.with(Cell::get) {
        return;
    }
    let Some(frame) = OUTBOX.with(|outbox| outbox.borrow_mut().pop_front()) else {
        return;
    };
    #[cfg(all(test, not(target_arch = "wasm32")))]
    SENT_LOG.with(|log| log.borrow_mut().push(frame.kind));
    // Off-wasm the send below cannot run: `send_frame` reaches the host through
    // the Tauri bridge, whose `wasm_bindgen` extern has no host implementation.
    // Record what was dropped and keep draining, so native tests can assert the
    // order frames were handed to the transport in — the property the queue
    // exists to guarantee.
    #[cfg(not(target_arch = "wasm32"))]
    {
        log::debug!(
            "voice: {} for host {} on {} was not sent — this build has no host \
             transport ({} bytes of payload)",
            frame.kind,
            frame.host_id,
            frame.stream,
            frame.payload.to_string().len()
        );
        pump_outbox();
    }
    #[cfg(target_arch = "wasm32")]
    OUTBOX_PUMPING.with(|flag| flag.set(true));
    #[cfg(target_arch = "wasm32")]
    spawn(async move {
        let result = crate::send::send_frame(
            &frame.host_id,
            frame.stream.clone(),
            frame.kind,
            &frame.payload,
        )
        .await;
        OUTBOX_PUMPING.with(|flag| flag.set(false));
        if let Err(error) = result {
            // `send_frame` already surfaced the transport error to the user.
            log::error!("voice: failed to send {}: {error}", frame.kind);
            // Only act if this frame still belongs to the live session. A late
            // failure from a session the user already ended must not delete
            // another session's queued frames or terminate it.
            if is_current(frame.generation)
                && matches!(frame.kind, FrameKind::VoiceStart | FrameKind::VoiceOffer)
            {
                OUTBOX.with(|outbox| {
                    outbox
                        .borrow_mut()
                        .retain(|queued| queued.generation != frame.generation);
                });
                end_with_failure(
                    VoiceEndReason::TransportFailed,
                    VoiceFailure {
                        stage: VoiceStage::Signaling,
                        message: format!("Voice could not reach the host: {error}"),
                        retryable: true,
                    },
                );
                return;
            }
        }
        pump_outbox();
    });
}

#[cfg(target_arch = "wasm32")]
fn spawn<F: std::future::Future<Output = ()> + 'static>(future: F) {
    wasm_bindgen_futures::spawn_local(future);
}

// Off-wasm there is no browser event loop, so spawned work is driven by a
// minimal cooperative executor.
//
// This exists so the native unit tests can exercise the *real* asynchronous
// paths — acquisition completing, an answer being applied, buffered ICE
// flushing — rather than only the synchronous edges around them. Those are
// exactly the paths where the ordering and teardown races live, so testing
// them against a no-op spawn would test nothing.
//
// A plain comment, not a doc comment: `///` on a macro invocation documents
// nothing and warns, and the next stage treats warnings as errors.
#[cfg(not(target_arch = "wasm32"))]
thread_local! {
    static PENDING_TASKS: RefCell<Vec<std::pin::Pin<Box<dyn std::future::Future<Output = ()>>>>> =
        const { RefCell::new(Vec::new()) };
    static DRIVING: Cell<bool> = const { Cell::new(false) };
}

#[cfg(not(target_arch = "wasm32"))]
fn spawn<F: std::future::Future<Output = ()> + 'static>(future: F) {
    PENDING_TASKS.with(|tasks| tasks.borrow_mut().push(Box::pin(future)));
    drive_pending_tasks();
}

/// Poll every queued task until none makes progress.
///
/// Re-entrancy matters: polling a task can spawn another, so the queue is
/// taken before polling and unfinished tasks are put back afterwards.
#[cfg(not(target_arch = "wasm32"))]
fn drive_pending_tasks() {
    if DRIVING.with(Cell::get) {
        return;
    }
    DRIVING.with(|flag| flag.set(true));
    let waker = std::task::Waker::noop();
    let mut context = std::task::Context::from_waker(waker);
    loop {
        let batch: Vec<_> = PENDING_TASKS.with(|tasks| std::mem::take(&mut *tasks.borrow_mut()));
        if batch.is_empty() {
            break;
        }
        let mut progressed = false;
        let mut still_pending = Vec::new();
        for mut task in batch {
            match task.as_mut().poll(&mut context) {
                std::task::Poll::Ready(()) => progressed = true,
                std::task::Poll::Pending => still_pending.push(task),
            }
        }
        PENDING_TASKS.with(|tasks| {
            let mut tasks = tasks.borrow_mut();
            // Newly spawned tasks are already in the queue; the unfinished
            // ones go behind them.
            tasks.extend(still_pending);
        });
        if !progressed {
            break;
        }
    }
    DRIVING.with(|flag| flag.set(false));
}

/// Test hook: resume tasks that were pending on an external completion, such
/// as `FakeMediaPlatform::resolve_start`.
#[cfg(all(test, not(target_arch = "wasm32")))]
pub(crate) fn pump_async_for_tests() {
    drive_pending_tasks();
}

// ── Focus / lifecycle guards ────────────────────────────────────────────────

/// Re-evaluate the bound target against authoritative app state and end the
/// session if it is no longer valid.
///
/// Ending here is synchronous, so the microphone stops in the same turn as the
/// change that invalidated it.
pub fn enforce_target(state: &AppState, focused: Option<&ActiveAgentRef>) {
    // Read the binding from the runtime, not the phase: a session whose phase
    // has moved on for display reasons still owns a microphone and must still
    // be torn down when its target goes away.
    let Some(bound) = read_state(|voice| voice.target.clone()) else {
        return;
    };
    if RUNTIME.with(|slot| slot.borrow().is_none()) {
        return;
    }

    // A different focused chat — including no chat at all, which is what Home
    // and a New Chat tab produce — ends the session.
    if focused != Some(&bound.agent) {
        end(VoiceEndReason::FocusedAgentChanged);
        return;
    }

    if !matches!(
        state.connection_status_for_host_untracked(&bound.agent.host_id),
        ConnectionStatus::Connected
    ) {
        end(VoiceEndReason::HostDisconnected);
        return;
    }

    let Some(info) = find_agent(state, &bound.agent) else {
        end(VoiceEndReason::AgentGone);
        return;
    };
    if info.fatal_error.is_some() {
        end(VoiceEndReason::AgentFatal);
        return;
    }
    if info.instance_stream != bound.instance_stream {
        end(VoiceEndReason::InstanceStreamChanged);
    }
}

/// Install the reactive guard. Tracks the focused agent, the agent list, and
/// connection status, so any route that invalidates the target — switching
/// tabs, closing the agent, a fatal error, a reconnect, a host drop — is
/// covered by one rule rather than by teardown calls scattered through the
/// dispatcher.
///
/// Portable: it uses only Leptos, so it is installed on every target rather
/// than gated to the browser.
pub fn install_guard(state: &AppState) {
    let state = state.clone();
    Effect::new(move |_| {
        let focused = state.active_agent.get();
        // Tracked reads: the guard must re-run when the agent list or a
        // connection status changes, not only when focus moves.
        state.agents.with(|_| ());
        state.connection_statuses.with(|_| ());
        enforce_target(&state, focused.as_ref());
    });
}

/// End voice because the window went away. The agent keeps running; only the
/// voice session stops.
pub fn end_for_lifecycle(hidden: bool) {
    if RUNTIME.with(|slot| slot.borrow().is_none()) {
        return;
    }
    end(if hidden {
        VoiceEndReason::WindowHidden
    } else {
        VoiceEndReason::PageTeardown
    });
}

/// Test observability. Components read the reactive state directly, so these
/// exist only so a test can ask a question without reaching into the signal.
#[cfg(test)]
pub(crate) fn is_engaged() -> bool {
    read_state(VoiceUiState::is_engaged)
}

#[cfg(test)]
pub(crate) fn phase() -> VoicePhase {
    read_state(|voice| voice.phase)
}

#[cfg(test)]
pub(crate) fn reset_for_tests() {
    #[cfg(not(target_arch = "wasm32"))]
    PENDING_TASKS.with(|tasks| tasks.borrow_mut().clear());
    if let Some(mut runtime) = RUNTIME.with(|slot| slot.borrow_mut().take()) {
        runtime.clear_deadline();
    }
    OUTBOX.with(|outbox| outbox.borrow_mut().clear());
    OUTBOX_PUMPING.with(|flag| flag.set(false));
    PLATFORM_FACTORY.with(|slot| *slot.borrow_mut() = None);
    #[cfg(not(target_arch = "wasm32"))]
    SENT_LOG.with(|log| log.borrow_mut().clear());
    VOICE_STATE.with(|signal| signal.set(VoiceUiState::default()));
}

#[cfg(all(test, not(target_arch = "wasm32")))]
pub(crate) fn sent_frame_kinds() -> Vec<FrameKind> {
    SENT_LOG.with(|log| log.borrow().clone())
}

#[cfg(all(test, not(target_arch = "wasm32")))]
pub(crate) fn runtime_is_live() -> bool {
    RUNTIME.with(|slot| slot.borrow().is_some())
}

/// The controller suite.
///
/// Native only, and not for convenience — these tests are written against the
/// host target's semantics and cannot hold on wasm:
///
/// - they drive spawned work with [`pump_async_for_tests`], which exists only
///   where `spawn` queues into the cooperative executor; on wasm it goes to the
///   browser event loop and a synchronous assertion after `start` would race it;
/// - they assert send *order* through `SENT_LOG`, which `pump_outbox` only
///   records off-wasm, because on wasm it really sends through the Tauri bridge;
/// - they rely on `random_bytes` minting deterministic ids from a counter.
///
/// Browser-side behaviour is covered separately by `wasm_tests` below and by
/// the mounted tests in `components::voice_layer`.
#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use crate::state::TabContent;
    use media::{FakeMediaPlatform, FakeMediaScript, MediaError, MediaEvent};
    use protocol::{
        AgentActivitySummaryState, AgentId, AgentOrigin, BackendKind, HostSettings,
        VoiceAvailability, VoiceUnavailableReason,
    };
    use session::{VoiceEndReason, VoiceFailure, VoiceStage};
    use std::cell::RefCell;

    const HOST: &str = "host-1";

    thread_local! {
        /// The platform the current test installed, so assertions can read the
        /// calls it recorded.
        static LAST_FAKE: RefCell<Option<Rc<FakeMediaPlatform>>> = const { RefCell::new(None) };
    }

    fn install_fake(script: FakeMediaScript) -> Rc<FakeMediaPlatform> {
        let platform = Rc::new(FakeMediaPlatform::with_script(script));
        LAST_FAKE.with(|slot| *slot.borrow_mut() = Some(platform.clone()));
        let for_factory = platform.clone();
        set_platform_factory(Rc::new(move || {
            for_factory.clone() as Rc<dyn MediaPlatform>
        }));
        platform
    }

    fn agent_ref(id: &str) -> ActiveAgentRef {
        ActiveAgentRef {
            host_id: HOST.to_owned(),
            agent_id: AgentId(id.to_owned()),
        }
    }

    fn agent_info(id: &str, stream: &str) -> AgentInfo {
        AgentInfo {
            host_id: HOST.to_owned(),
            agent_id: AgentId(id.to_owned()),
            name: format!("Agent {id}"),
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
            instance_stream: StreamPath(stream.to_owned()),
            started: true,
            fatal_error: None,
            activity_summary: AgentActivitySummaryState::Disabled,
        }
    }

    /// A connected host that has declared voice available, with two live agents.
    fn ready_state() -> AppState {
        let state = AppState::new();
        state.connection_statuses.update(|statuses| {
            statuses.insert(HOST.to_owned(), ConnectionStatus::Connected);
        });
        state.host_settings_by_host.update(|settings| {
            settings.insert(
                HOST.to_owned(),
                HostSettings {
                    voice: protocol::VoiceSettings {
                        enabled: true,
                        availability: VoiceAvailability::Available {
                            direct_connections_only: true,
                        },
                        ..Default::default()
                    },
                    ..Default::default()
                },
            );
        });
        state.agents.update(|agents| {
            agents.push(agent_info("a", "/agent/a"));
            agents.push(agent_info("b", "/agent/b"));
        });
        state
    }

    fn start_for(state: &AppState, id: &str) -> VoiceTarget {
        let target = availability(state, Some(&agent_ref(id))).expect("target available");
        start(target.clone());
        target
    }

    fn with_voice(body: impl FnOnce(AppState)) {
        with_scripted_voice(FakeMediaScript::default(), body);
    }

    fn with_scripted_voice(script: FakeMediaScript, body: impl FnOnce(AppState)) {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            reset_for_tests();
            install_fake(script);
            body(ready_state());
            reset_for_tests();
        });
    }

    fn fake() -> Rc<FakeMediaPlatform> {
        LAST_FAKE
            .with(|slot| slot.borrow().clone())
            .expect("a fake media platform is installed")
    }

    /// Drive the reducer to `Live` without a browser media stack.
    ///
    /// The immediately-ready fake already advances past `RequestingMic`, so
    /// the grant is best-effort here and only `connected` is required.
    fn force_live() -> VoiceGeneration {
        let generation = read_state(|voice| voice.generation);
        update_state(|voice| voice.mic_granted(generation, EffectiveAudio::default()));
        update_state(|voice| voice.connected(generation));
        generation
    }

    fn transcript_frame(
        session: &VoiceSessionKey,
        state: protocol::VoiceSessionState,
        speaker: protocol::VoiceTranscriptSpeaker,
        text: &str,
        is_final: bool,
    ) -> Envelope {
        Envelope::from_payload(
            session.stream.clone(),
            FrameKind::VoiceState,
            0,
            &protocol::VoiceStatePayload {
                session_id: protocol::VoiceSessionId(session.session_id.clone()),
                state,
                progress: None,
                caption: Some(text.to_owned()),
                transcript: Some(protocol::VoiceTranscript {
                    speaker,
                    text: text.to_owned(),
                    is_final,
                }),
                ended_reason: None,
            },
        )
        .expect("encode transcript frame")
    }

    fn state_frame(session: &VoiceSessionKey, state: protocol::VoiceSessionState) -> Envelope {
        Envelope::from_payload(
            session.stream.clone(),
            FrameKind::VoiceState,
            0,
            &protocol::VoiceStatePayload {
                session_id: protocol::VoiceSessionId(session.session_id.clone()),
                state,
                progress: None,
                caption: None,
                transcript: None,
                ended_reason: None,
            },
        )
        .expect("encode state frame")
    }

    #[test]
    fn availability_reports_each_blocking_condition_with_its_own_reason() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            reset_for_tests();
            let state = ready_state();

            assert!(matches!(
                availability(&state, None),
                Err(VoiceUnavailable::NoAgent)
            ));
            assert!(availability(&state, Some(&agent_ref("a"))).is_ok());

            // A host that has not yet reported settings has not said yes.
            let quiet = AppState::new();
            quiet.connection_statuses.update(|statuses| {
                statuses.insert(HOST.to_owned(), ConnectionStatus::Connected);
            });
            assert!(matches!(
                availability(&quiet, Some(&agent_ref("a"))),
                Err(VoiceUnavailable::HostReported(_))
            ));

            // A host that says no is quoted, not overridden.
            state.host_settings_by_host.update(|settings| {
                settings.insert(
                    HOST.to_owned(),
                    HostSettings {
                        voice: protocol::VoiceSettings {
                            availability: VoiceAvailability::Unavailable {
                                reason: VoiceUnavailableReason::NoReachableCandidate,
                            },
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                );
            });
            let refused = availability(&state, Some(&agent_ref("a"))).expect_err("host said no");
            assert!(
                refused.reason().contains("direct network route"),
                "the host's reason must reach the user, got {:?}",
                refused.reason()
            );

            reset_for_tests();
        });
    }

    #[test]
    fn a_disconnected_host_or_dead_agent_blocks_starting() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            reset_for_tests();
            let state = ready_state();

            state.agents.update(|agents| {
                agents[0].fatal_error = Some("backend crashed".to_owned());
                agents[1].started = false;
            });
            assert!(matches!(
                availability(&state, Some(&agent_ref("a"))),
                Err(VoiceUnavailable::AgentTerminated)
            ));
            assert!(matches!(
                availability(&state, Some(&agent_ref("b"))),
                Err(VoiceUnavailable::AgentNotStarted)
            ));

            state.connection_statuses.update(|statuses| {
                statuses.insert(HOST.to_owned(), ConnectionStatus::Disconnected);
            });
            assert!(matches!(
                availability(&state, Some(&agent_ref("a"))),
                Err(VoiceUnavailable::HostDisconnected)
            ));
            reset_for_tests();
        });
    }

    #[test]
    fn starting_sends_voice_start_first_on_the_sessions_own_stream() {
        with_voice(|state| {
            start_for(&state, "a");
            assert_eq!(
                sent_frame_kinds().first(),
                Some(&FrameKind::VoiceStart),
                "VoiceStart opens the stream and must precede everything else"
            );
            assert_eq!(
                sent_frame_kinds(),
                vec![FrameKind::VoiceStart, FrameKind::VoiceOffer],
                "and the offer follows it, in that order, on the same stream"
            );
            let stream = read_state(|voice| voice.session.clone()).expect("session minted");
            assert!(
                stream.stream.0.starts_with("/voice/"),
                "voice never rides an agent stream, got {}",
                stream.stream
            );
            assert_eq!(
                phase(),
                VoicePhase::Negotiating,
                "the microphone is granted and the offer is out; the host has not answered yet"
            );
        });
    }

    #[test]
    fn a_second_start_while_engaged_is_refused_rather_than_retargeting() {
        with_voice(|state| {
            start_for(&state, "a");
            let busy = availability(&state, Some(&agent_ref("b")))
                .expect_err("another chat cannot start while one is live");
            assert!(matches!(busy, VoiceUnavailable::BusyElsewhere));
            assert_eq!(
                read_state(|voice| voice.target.clone().map(|t| t.agent.agent_id.0)),
                Some("a".to_owned()),
                "the original binding must survive an attempt to start elsewhere"
            );
        });
    }

    #[test]
    fn switching_the_focused_agent_stops_media_and_sends_stop() {
        with_voice(|state| {
            let platform = LAST_FAKE.with(|slot| slot.borrow().clone()).expect("fake");
            start_for(&state, "a");
            assert_eq!(platform.calls().stops, 0);

            enforce_target(&state, Some(&agent_ref("b")));

            assert_eq!(
                platform.calls().stops,
                1,
                "the microphone must stop as part of the switch, not after a round trip"
            );
            assert_eq!(
                read_state(|voice| voice.ended_reason),
                Some(VoiceEndReason::FocusedAgentChanged)
            );
            assert_eq!(
                sent_frame_kinds(),
                vec![
                    FrameKind::VoiceStart,
                    FrameKind::VoiceOffer,
                    FrameKind::VoiceStop
                ]
            );
        });
    }

    #[test]
    fn losing_the_focused_chat_entirely_also_ends_voice() {
        with_voice(|state| {
            let platform = LAST_FAKE.with(|slot| slot.borrow().clone()).expect("fake");
            start_for(&state, "a");
            // Home or a New Chat tab yields no focused agent at all.
            enforce_target(&state, None);
            assert_eq!(platform.calls().stops, 1);
            assert_eq!(
                read_state(|voice| voice.ended_reason),
                Some(VoiceEndReason::FocusedAgentChanged)
            );
        });
    }

    #[test]
    fn an_agent_reconnect_ends_voice_even_though_the_agent_id_is_unchanged() {
        with_voice(|state| {
            let platform = LAST_FAKE.with(|slot| slot.borrow().clone()).expect("fake");
            start_for(&state, "a");
            state.agents.update(|agents| {
                agents[0].instance_stream = StreamPath("/agent/a-generation-2".to_owned());
            });
            enforce_target(&state, Some(&agent_ref("a")));
            assert_eq!(platform.calls().stops, 1);
            assert_eq!(
                read_state(|voice| voice.ended_reason),
                Some(VoiceEndReason::InstanceStreamChanged),
                "the connection generation is part of the binding"
            );
        });
    }

    #[test]
    fn a_closed_agent_a_fatal_agent_and_a_dropped_host_each_end_voice() {
        for (mutate, expected) in [
            (
                Box::new(|state: &AppState| {
                    state.agents.update(|agents| agents.clear());
                }) as Box<dyn Fn(&AppState)>,
                VoiceEndReason::AgentGone,
            ),
            (
                Box::new(|state: &AppState| {
                    state
                        .agents
                        .update(|agents| agents[0].fatal_error = Some("boom".to_owned()));
                }),
                VoiceEndReason::AgentFatal,
            ),
            (
                Box::new(|state: &AppState| {
                    state.connection_statuses.update(|statuses| {
                        statuses.insert(HOST.to_owned(), ConnectionStatus::Disconnected);
                    });
                }),
                VoiceEndReason::HostDisconnected,
            ),
        ] {
            with_voice(|state| {
                let platform = LAST_FAKE.with(|slot| slot.borrow().clone()).expect("fake");
                start_for(&state, "a");
                mutate(&state);
                enforce_target(&state, Some(&agent_ref("a")));
                assert_eq!(
                    platform.calls().stops,
                    1,
                    "expected teardown for {expected:?}"
                );
                assert_eq!(read_state(|voice| voice.ended_reason), Some(expected));
            });
        }
    }

    #[test]
    fn repeated_teardown_stops_media_once_and_sends_one_stop() {
        with_voice(|state| {
            let platform = LAST_FAKE.with(|slot| slot.borrow().clone()).expect("fake");
            start_for(&state, "a");
            end(VoiceEndReason::UserRequested);
            end(VoiceEndReason::HostDisconnected);
            enforce_target(&state, None);
            assert_eq!(platform.calls().stops, 1);
            assert_eq!(
                sent_frame_kinds()
                    .iter()
                    .filter(|kind| **kind == FrameKind::VoiceStop)
                    .count(),
                1
            );
        });
    }

    #[test]
    fn hiding_the_window_ends_voice_but_leaves_the_agent_alone() {
        with_voice(|state| {
            let platform = LAST_FAKE.with(|slot| slot.borrow().clone()).expect("fake");
            start_for(&state, "a");
            let agents_before = state.agents.get_untracked().len();

            end_for_lifecycle(true);

            assert_eq!(platform.calls().stops, 1);
            assert_eq!(
                read_state(|voice| voice.ended_reason),
                Some(VoiceEndReason::WindowHidden)
            );
            assert_eq!(
                state.agents.get_untracked().len(),
                agents_before,
                "ending voice must not touch the agent"
            );
            // Idle: hiding again is a no-op, not a second stop.
            end_for_lifecycle(true);
            assert_eq!(platform.calls().stops, 1);
        });
    }

    #[test]
    fn voice_never_touches_the_chat_composer_or_agent_stream() {
        with_voice(|state| {
            let tab = state
                .open_tab(
                    TabContent::chat_with_agent(agent_ref("a")),
                    "A".to_owned(),
                    true,
                )
                .expect("chat tab opens");
            let composer = state.composer_for(tab);
            composer.text.set("a draft the user typed".to_owned());

            start_for(&state, "a");
            end(VoiceEndReason::UserRequested);

            assert_eq!(
                composer.text.get_untracked(),
                "a draft the user typed",
                "entering and leaving voice must not disturb the draft"
            );
            for kind in sent_frame_kinds() {
                assert!(
                    matches!(
                        kind,
                        FrameKind::VoiceStart
                            | FrameKind::VoiceOffer
                            | FrameKind::VoiceIceCandidate
                            | FrameKind::VoiceIceCandidatesComplete
                            | FrameKind::VoiceStop
                    ),
                    "voice must not emit agent-protocol frames, saw {kind}"
                );
            }
        });
    }

    #[test]
    fn a_frame_for_another_session_is_ignored_and_cannot_end_ours() {
        with_voice(|state| {
            start_for(&state, "a");
            let ours = read_state(|voice| voice.session.clone()).expect("session");

            let stray = Envelope::from_payload(
                wire::voice_stream("someone-elses"),
                FrameKind::VoiceState,
                0,
                &protocol::VoiceStatePayload {
                    session_id: protocol::VoiceSessionId("someone-elses".to_owned()),
                    state: protocol::VoiceSessionState::Ended,
                    progress: None,
                    caption: None,
                    transcript: None,
                    ended_reason: Some(protocol::VoiceStopReason::TimedOut),
                },
            )
            .expect("encode stray frame");

            assert!(
                handle_inbound(HOST, &stray),
                "a /voice/ frame is voice's to consume even when it is not ours"
            );
            assert!(is_engaged(), "a stray session's teardown must not end ours");
            assert_eq!(read_state(|voice| voice.session.clone()), Some(ours));
        });
    }

    #[test]
    fn the_host_ending_the_session_tears_down_locally() {
        with_voice(|state| {
            let platform = LAST_FAKE.with(|slot| slot.borrow().clone()).expect("fake");
            start_for(&state, "a");
            let ours = read_state(|voice| voice.session.clone()).expect("session");

            let ended = Envelope::from_payload(
                ours.stream.clone(),
                FrameKind::VoiceState,
                0,
                &protocol::VoiceStatePayload {
                    session_id: protocol::VoiceSessionId(ours.session_id.clone()),
                    state: protocol::VoiceSessionState::Ended,
                    progress: None,
                    caption: None,
                    transcript: None,
                    ended_reason: Some(protocol::VoiceStopReason::TimedOut),
                },
            )
            .expect("encode ended frame");

            assert!(handle_inbound(HOST, &ended));
            assert_eq!(platform.calls().stops, 1);
            assert!(!is_engaged());
        });
    }

    #[test]
    fn a_frame_whose_payload_disagrees_with_its_stream_ends_the_session() {
        with_voice(|state| {
            start_for(&state, "a");
            let ours = read_state(|voice| voice.session.clone()).expect("session");

            let spoofed = Envelope::from_payload(
                ours.stream.clone(),
                FrameKind::VoiceReady,
                0,
                &protocol::VoiceReadyPayload {
                    session_id: protocol::VoiceSessionId("a-different-session".to_owned()),
                    target: protocol::VoiceTarget {
                        agent_id: AgentId("a".to_owned()),
                        instance_stream: StreamPath("/agent/a".to_owned()),
                    },
                    direct_connections_only: true,
                    expires_after_seconds: 60,
                },
            )
            .expect("encode spoofed frame");

            assert!(handle_inbound(HOST, &spoofed));
            assert!(
                !is_engaged(),
                "a payload that disagrees with its stream is not trusted"
            );
        });
    }

    #[test]
    fn progress_is_only_ever_a_projection_of_a_server_sent_event() {
        with_voice(|state| {
            start_for(&state, "a");
            // Force the state machine to Live without a browser: the media
            // events that would do this need a peer connection.
            force_live();

            let ours = read_state(|voice| voice.session.clone()).expect("session");
            let frame = |seq: u64, kind: protocol::VoiceAgentProgressKind| {
                Envelope::from_payload(
                    ours.stream.clone(),
                    FrameKind::VoiceState,
                    0,
                    &protocol::VoiceStatePayload {
                        session_id: protocol::VoiceSessionId(ours.session_id.clone()),
                        state: protocol::VoiceSessionState::AgentWorking,
                        progress: Some(protocol::VoiceAgentProgress {
                            source_seq: seq,
                            source_kind: kind,
                        }),
                        caption: None,
                        transcript: None,
                        ended_reason: None,
                    },
                )
                .expect("encode state frame")
            };

            assert!(handle_inbound(
                HOST,
                &frame(7, protocol::VoiceAgentProgressKind::ToolStarted)
            ));
            assert_eq!(read_state(|voice| voice.progress.len()), 1);
            assert!(read_state(|voice| voice.tool_notice.is_some()));

            // A replay of the same agent event must not produce a second line.
            assert!(handle_inbound(
                HOST,
                &frame(7, protocol::VoiceAgentProgressKind::ToolStarted)
            ));
            assert_eq!(read_state(|voice| voice.progress.len()), 1);

            // A non-tool event clears the tool banner rather than leaving it stale.
            assert!(handle_inbound(
                HOST,
                &frame(8, protocol::VoiceAgentProgressKind::ResponseCompleted)
            ));
            assert_eq!(read_state(|voice| voice.progress.len()), 2);
            assert!(read_state(|voice| voice.tool_notice.is_none()));

            // Nothing in the client can add a line without a server frame.
            let before = read_state(|voice| voice.progress.len());
            enforce_target(&state, Some(&agent_ref("a")));
            assert_eq!(read_state(|voice| voice.progress.len()), before);
        });
    }

    #[test]
    fn a_fatal_host_error_ends_voice_and_surfaces_the_hosts_own_message() {
        with_voice(|state| {
            start_for(&state, "a");
            let ours = read_state(|voice| voice.session.clone()).expect("session");
            let error = Envelope::from_payload(
                ours.stream.clone(),
                FrameKind::VoiceError,
                0,
                &protocol::VoiceErrorPayload {
                    session_id: protocol::VoiceSessionId(ours.session_id.clone()),
                    code: protocol::VoiceErrorCode::ProviderUnavailable,
                    message: "Bedrock credentials could not be resolved".to_owned(),
                    fatal: true,
                },
            )
            .expect("encode error frame");

            assert!(handle_inbound(HOST, &error));
            assert!(!is_engaged());
            assert_eq!(
                read_state(|voice| voice.failure.as_ref().map(|f| f.message.clone())),
                Some("Bedrock credentials could not be resolved".to_owned()),
                "the host's own words reach the user"
            );
            assert!(
                crate::components::header::current_user_error()
                    .is_some_and(|message| message.contains("Bedrock credentials")),
                "a fatal voice error is also raised as a user-facing error"
            );
        });
    }

    #[test]
    fn with_no_media_backend_starting_fails_visibly_instead_of_doing_nothing() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.with(|| {
            reset_for_tests();
            let state = ready_state();
            // No platform factory installed at all.
            let target = availability(&state, Some(&agent_ref("a"))).expect("available");
            start(target);

            assert_eq!(phase(), VoicePhase::Failed);
            assert!(
                read_state(|voice| voice.failure.is_some()),
                "a missing audio backend must be a visible failure, never a no-op button"
            );
            assert!(
                sent_frame_kinds().is_empty(),
                "nothing is sent without media"
            );
            reset_for_tests();
        });
    }

    /// The server parses the last stream segment with `Uuid::parse_str` and
    /// rejects anything else — on the *host* stream, where this controller
    /// would never see it. A non-UUID id therefore fails silently and hangs.
    #[test]
    fn the_session_id_is_a_uuid_v4_because_the_host_parses_it_as_one() {
        let id = new_session_id().expect("session id mints");
        let groups: Vec<&str> = id.split('-').collect();
        assert_eq!(groups.len(), 5, "expected 8-4-4-4-12, got {id}");
        assert_eq!(
            groups.iter().map(|group| group.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12],
            "{id}"
        );
        assert!(
            id.chars().all(|c| c.is_ascii_hexdigit() || c == '-'),
            "{id} must be hex and dashes only"
        );
        assert_eq!(
            groups[2].chars().next(),
            Some('4'),
            "RFC 4122 version nibble must be 4 in {id}"
        );
        assert!(
            matches!(groups[3].chars().next(), Some('8' | '9' | 'a' | 'b')),
            "RFC 4122 variant bits must be 10 in {id}"
        );
        assert_ne!(
            id,
            new_session_id().expect("second id"),
            "ids must be distinct"
        );
    }

    #[test]
    fn the_voice_stream_path_is_the_uuid_and_nothing_else() {
        with_voice(|state| {
            start_for(&state, "a");
            let session = read_state(|voice| voice.session.clone()).expect("session");
            assert_eq!(session.stream.0, format!("/voice/{}", session.session_id));
            assert_eq!(
                session.stream.0.split('/').count(),
                3,
                "the host splits on '/' and requires exactly three segments"
            );
        });
    }

    /// The permission-prompt race. Teardown lands while `getUserMedia` is
    /// outstanding; when the prompt is finally answered the controller must
    /// not adopt the capture it no longer owns.
    #[test]
    fn a_grant_that_lands_after_teardown_does_not_resurrect_a_session() {
        with_scripted_voice(
            FakeMediaScript {
                start_pending: true,
                ..FakeMediaScript::default()
            },
            |state| {
                start_for(&state, "a");
                let platform = fake();
                assert!(!platform.is_cancelled());

                // The user switches agents while the sheet is open.
                enforce_target(&state, Some(&agent_ref("b")));
                assert!(
                    platform.is_cancelled(),
                    "teardown must reach a start that is still suspended"
                );
                assert_eq!(platform.calls().stops, 1);
                assert!(!runtime_is_live());

                // The prompt is answered afterwards.
                platform.resolve_start();
                pump_async_for_tests();
                assert!(!is_engaged(), "a stale grant must not revive the session");
                assert!(!runtime_is_live());
                assert_eq!(
                    read_state(|voice| voice.ended_reason),
                    Some(VoiceEndReason::FocusedAgentChanged)
                );
            },
        );
    }

    /// A non-fatal problem must leave the session engaged. If it moved the
    /// phase to a terminal state the strip would hide End while the microphone
    /// was still open, and the runtime would be unreachable.
    #[test]
    fn a_nonfatal_host_error_warns_without_dropping_ownership() {
        with_voice(|state| {
            start_for(&state, "a");
            force_live();
            let session = read_state(|voice| voice.session.clone()).expect("session");
            let error = Envelope::from_payload(
                session.stream.clone(),
                FrameKind::VoiceError,
                0,
                &protocol::VoiceErrorPayload {
                    session_id: protocol::VoiceSessionId(session.session_id.clone()),
                    code: protocol::VoiceErrorCode::ToolBusy,
                    message: "the agent is already handling a request".to_owned(),
                    fatal: false,
                },
            )
            .expect("encode nonfatal error");

            assert!(handle_inbound(HOST, &error));

            assert!(is_engaged(), "a non-fatal error must not end the session");
            assert!(runtime_is_live(), "media ownership must be retained");
            assert_eq!(fake().calls().stops, 0);
            assert_eq!(
                read_state(|voice| voice.warning.as_ref().map(|w| w.message.clone())),
                Some("the agent is already handling a request".to_owned()),
                "the host's own words are shown as a warning"
            );
            assert_eq!(
                read_state(|voice| voice.failure.clone()),
                None,
                "a survivable problem is not a terminal failure"
            );

            // And the session is still endable, which is the whole point.
            end(VoiceEndReason::UserRequested);
            assert_eq!(fake().calls().stops, 1);
            assert!(!runtime_is_live());
        });
    }

    /// Mute is reachable while the permission sheet is open, where the browser
    /// platform has no track yet.
    #[test]
    fn a_refused_mute_is_a_warning_not_a_teardown() {
        with_scripted_voice(
            FakeMediaScript {
                mute_error: Some(MediaError::media("the microphone is not open yet")),
                ..FakeMediaScript::default()
            },
            |state| {
                start_for(&state, "a");
                toggle_mute();
                assert!(is_engaged());
                assert!(runtime_is_live());
                assert_eq!(fake().calls().stops, 0);
                assert!(read_state(|voice| voice.warning.is_some()));
                assert!(
                    !read_state(|voice| voice.muted),
                    "a refused mute must not claim the microphone is muted"
                );
            },
        );
    }

    /// The host terminates a session that receives a candidate before it has
    /// accepted the offer, so candidates gathered during acquisition are held.
    #[test]
    fn local_candidates_never_precede_the_offer_on_the_wire() {
        with_scripted_voice(
            FakeMediaScript {
                start_pending: true,
                ..FakeMediaScript::default()
            },
            |state| {
                start_for(&state, "a");
                let platform = fake();

                // ICE gathering begins while acquisition is still outstanding.
                platform.emit(MediaEvent::LocalIceCandidate(media::LocalIceCandidate {
                    candidate: "candidate:1 1 udp".to_owned(),
                    sdp_mid: Some("0".to_owned()),
                    sdp_m_line_index: Some(0),
                }));
                platform.emit(MediaEvent::LocalIceComplete);
                assert_eq!(
                    sent_frame_kinds(),
                    vec![FrameKind::VoiceStart],
                    "no candidate may reach the wire before the offer"
                );

                platform.resolve_start();
                pump_async_for_tests();
                assert_eq!(
                    sent_frame_kinds(),
                    vec![
                        FrameKind::VoiceStart,
                        FrameKind::VoiceOffer,
                        FrameKind::VoiceIceCandidate,
                        FrameKind::VoiceIceCandidatesComplete,
                    ],
                    "buffered candidates flush in order behind the offer"
                );
            },
        );
    }

    /// `addIceCandidate` before `setRemoteDescription` resolves throws, and the
    /// discarded candidate may have been the only viable direct pair.
    #[test]
    fn remote_candidates_wait_for_the_answer_to_be_applied() {
        with_voice(|state| {
            start_for(&state, "a");
            let session = read_state(|voice| voice.session.clone()).expect("session");

            let candidates = Envelope::from_payload(
                session.stream.clone(),
                FrameKind::VoiceIceCandidate,
                0,
                &protocol::VoiceIceCandidatePayload {
                    session_id: protocol::VoiceSessionId(session.session_id.clone()),
                    candidates: vec![protocol::VoiceIceCandidate {
                        candidate: "candidate:9 1 udp".to_owned(),
                        sdp_mid: Some("0".to_owned()),
                        sdp_m_line_index: Some(0),
                    }],
                },
            )
            .expect("encode candidates");

            assert!(handle_inbound(HOST, &candidates));
            assert!(
                fake().calls().remote_candidates.is_empty(),
                "a candidate must not be applied before the answer is"
            );
            assert_eq!(
                RUNTIME.with(|slot| slot
                    .borrow()
                    .as_ref()
                    .map(|runtime| runtime.pending_remote_candidates.len())),
                Some(1),
                "it is buffered, not dropped — it may be the only direct pair"
            );
        });
    }

    /// A teardown must drop only its own queued frames. Deleting the whole
    /// queue would let a late failure from session A kill session B.
    #[test]
    fn ending_a_session_drops_its_own_queued_frames_and_nothing_else() {
        with_voice(|state| {
            // Hold the pump so the queue is observable; natively it otherwise
            // drains to the transport immediately.
            OUTBOX_PUMPING.with(|flag| flag.set(true));
            start_for(&state, "a");
            let live = read_state(|voice| voice.generation);
            let foreign = VoiceGeneration(live.0.wrapping_add(7));
            OUTBOX.with(|outbox| {
                outbox.borrow_mut().push_back(OutboundFrame {
                    generation: foreign,
                    host_id: HOST.to_owned(),
                    stream: StreamPath("/voice/somebody-else".to_owned()),
                    kind: FrameKind::VoiceIceCandidate,
                    payload: serde_json::Value::Null,
                });
            });

            end(VoiceEndReason::UserRequested);

            let queued: Vec<(VoiceGeneration, FrameKind)> = OUTBOX.with(|outbox| {
                outbox
                    .borrow()
                    .iter()
                    .map(|frame| (frame.generation, frame.kind))
                    .collect()
            });
            assert!(
                queued.contains(&(foreign, FrameKind::VoiceIceCandidate)),
                "another session's queued frame must survive this teardown: {queued:?}"
            );
            assert!(
                !queued.contains(&(live, FrameKind::VoiceStart)),
                "the ended session's unsent frames are dropped: {queued:?}"
            );
            assert!(
                queued.contains(&(live, FrameKind::VoiceStop)),
                "but its stop still goes out so the host does not wait out the lease"
            );
            OUTBOX_PUMPING.with(|flag| flag.set(false));
        });
    }

    /// ICE `disconnected` is the recoverable state; only `failed`/`closed` end
    /// a session. Treating them alike drops voice on any brief roam.
    #[test]
    fn an_unstable_connection_warns_but_a_failed_one_ends() {
        with_voice(|state| {
            start_for(&state, "a");
            force_live();
            let platform = fake();

            platform.emit(MediaEvent::ConnectionUnstable);
            assert!(is_engaged(), "`disconnected` routinely recovers");
            assert!(read_state(|voice| voice.warning.is_some()));
            assert_eq!(platform.calls().stops, 0);

            // Recovery clears the warning.
            platform.emit(MediaEvent::Connected);
            assert!(read_state(|voice| voice.warning.is_none()));

            platform.emit(MediaEvent::Disconnected("failed".to_owned()));
            assert!(!is_engaged());
            assert!(!runtime_is_live());
            assert_eq!(platform.calls().stops, 1);
        });
    }

    /// Barge-in: output stops at once, the microphone stays live, and the next
    /// agent utterance is audible again.
    #[test]
    fn silencing_output_stops_audio_and_the_next_utterance_recovers() {
        with_voice(|state| {
            start_for(&state, "a");
            force_live();
            let session = read_state(|voice| voice.session.clone()).expect("session");
            let platform = fake();

            silence_output();
            assert_eq!(platform.calls().silences, 1);
            assert!(
                read_state(|voice| voice.playback_blocked),
                "the recovery control must be offered after silencing"
            );
            assert!(is_engaged(), "silencing output must not end the session");
            assert!(
                platform.calls().muted.is_empty(),
                "the microphone stays live so the user can keep talking"
            );

            // The agent starts a new utterance.
            assert!(handle_inbound(
                HOST,
                &state_frame(&session, protocol::VoiceSessionState::Speaking)
            ));
            assert_eq!(
                platform.calls().resumes,
                1,
                "playback must be restored, or the session is silent for good"
            );
        });
    }

    /// A voice frame rejected before the host had a session to answer on comes
    /// back as a `CommandError` on the host stream. Without correlation the
    /// session hangs to its deadline showing nothing.
    #[test]
    fn a_host_stream_rejection_of_a_voice_frame_is_surfaced_and_ends_the_session() {
        with_voice(|state| {
            start_for(&state, "a");
            let ours = read_state(|voice| voice.session.clone()).expect("session");
            assert!(handle_command_error(
                HOST,
                &ours.stream,
                FrameKind::VoiceStart,
                "voice stream contains invalid session UUID"
            ));
            assert!(!is_engaged());
            assert!(!runtime_is_live());
            assert_eq!(fake().calls().stops, 1);
            let shown = read_state(|voice| voice.failure.as_ref().map(|f| f.message.clone()))
                .expect("a rejected admission must be visible");
            assert!(shown.contains("invalid session UUID"), "{shown}");
            assert!(
                crate::components::header::current_user_error()
                    .is_some_and(|message| message.contains("invalid session UUID"))
            );
        });
    }

    #[test]
    fn a_command_error_is_ignored_unless_it_names_this_exact_voice_stream() {
        with_voice(|state| {
            start_for(&state, "a");
            let ours = read_state(|voice| voice.session.clone()).expect("session");

            assert!(
                !handle_command_error(HOST, &ours.stream, FrameKind::SendMessage, "unrelated"),
                "non-voice frames stay with the ordinary error surface"
            );
            assert!(
                !handle_command_error(
                    "other-host",
                    &ours.stream,
                    FrameKind::VoiceStart,
                    "not ours"
                ),
                "another host's voice rejection is not this session's"
            );
            // The case host+kind scoping alone could not tell apart: a late
            // rejection of a *previous* session's frame on this same host.
            assert!(
                !handle_command_error(
                    HOST,
                    &wire::voice_stream("a-session-that-already-ended"),
                    FrameKind::VoiceStart,
                    "stale"
                ),
                "a rejection for another voice stream must not end the live session"
            );
            assert!(is_engaged(), "none of those touched the live session");
            assert!(runtime_is_live());
        });
    }

    /// A `VoiceReady` naming a different agent means the two sides disagree
    /// about what the session is bound to.
    #[test]
    fn a_ready_for_a_different_target_ends_the_session() {
        with_voice(|state| {
            start_for(&state, "a");
            let session = read_state(|voice| voice.session.clone()).expect("session");
            let ready = Envelope::from_payload(
                session.stream.clone(),
                FrameKind::VoiceReady,
                0,
                &protocol::VoiceReadyPayload {
                    session_id: protocol::VoiceSessionId(session.session_id.clone()),
                    target: protocol::VoiceTarget {
                        agent_id: AgentId("b".to_owned()),
                        instance_stream: StreamPath("/agent/b".to_owned()),
                    },
                    direct_connections_only: true,
                    expires_after_seconds: 60,
                },
            )
            .expect("encode ready");

            assert!(handle_inbound(HOST, &ready));
            assert!(!is_engaged());
            assert!(!runtime_is_live());
            assert!(
                read_state(|voice| voice.failure.is_some()),
                "a mismatched binding must be visible, not silently accepted"
            );
        });
    }

    /// The host answers as soon as its provider is up, which can easily be
    /// while the permission sheet is still open.
    #[test]
    fn a_ready_that_arrives_during_the_permission_prompt_is_accepted() {
        with_scripted_voice(
            FakeMediaScript {
                start_pending: true,
                ..FakeMediaScript::default()
            },
            |state| {
                let target = start_for(&state, "a");
                assert_eq!(phase(), VoicePhase::RequestingMic);
                let session = read_state(|voice| voice.session.clone()).expect("session");
                let ready = Envelope::from_payload(
                    session.stream.clone(),
                    FrameKind::VoiceReady,
                    0,
                    &protocol::VoiceReadyPayload {
                        session_id: protocol::VoiceSessionId(session.session_id.clone()),
                        target: protocol::VoiceTarget {
                            agent_id: target.agent.agent_id.clone(),
                            instance_stream: target.instance_stream.clone(),
                        },
                        direct_connections_only: true,
                        expires_after_seconds: 60,
                    },
                )
                .expect("encode ready");

                assert!(handle_inbound(HOST, &ready));
                assert!(
                    read_state(|voice| voice.host_admitted),
                    "a Ready during the prompt must not be dropped"
                );
                assert!(is_engaged());
            },
        );
    }

    #[test]
    fn a_ready_with_no_lease_is_refused() {
        with_voice(|state| {
            let target = start_for(&state, "a");
            let session = read_state(|voice| voice.session.clone()).expect("session");
            let ready = Envelope::from_payload(
                session.stream.clone(),
                FrameKind::VoiceReady,
                0,
                &protocol::VoiceReadyPayload {
                    session_id: protocol::VoiceSessionId(session.session_id.clone()),
                    target: protocol::VoiceTarget {
                        agent_id: target.agent.agent_id.clone(),
                        instance_stream: target.instance_stream.clone(),
                    },
                    direct_connections_only: true,
                    expires_after_seconds: 0,
                },
            )
            .expect("encode ready");
            assert!(handle_inbound(HOST, &ready));
            assert!(!is_engaged());
        });
    }

    /// Page teardown must release the microphone and tell the host, and must
    /// leave the agent running.
    #[test]
    fn page_hide_releases_the_microphone_and_leaves_the_agent_running() {
        with_voice(|state| {
            start_for(&state, "a");
            let agents_before = state.agents.get_untracked().len();

            end_for_lifecycle(false);

            assert_eq!(fake().calls().stops, 1);
            assert!(!runtime_is_live());
            assert_eq!(
                read_state(|voice| voice.ended_reason),
                Some(VoiceEndReason::PageTeardown)
            );
            assert!(
                sent_frame_kinds().contains(&FrameKind::VoiceStop),
                "the host is told, so it does not wait out its own lease"
            );
            assert_eq!(state.agents.get_untracked().len(), agents_before);
        });
    }

    /// The negotiation deadline exists because a locally-admitted frame can
    /// still be lost. Its message must distinguish "the host never answered"
    /// from "the host answered but audio never connected".
    #[test]
    fn the_negotiation_deadline_explains_which_half_of_setup_failed() {
        with_voice(|state| {
            start_for(&state, "a");
            let generation = read_state(|voice| voice.generation);

            on_negotiation_deadline(generation);
            assert!(!is_engaged());
            assert!(!runtime_is_live());
            let message = read_state(|voice| voice.failure.as_ref().map(|f| f.message.clone()))
                .expect("a timed-out session must say why");
            assert!(
                message.contains("did not answer"),
                "an unadmitted session blames delivery, got {message:?}"
            );
        });

        with_voice(|state| {
            start_for(&state, "a");
            let generation = read_state(|voice| voice.generation);
            update_state(|voice| voice.ready(generation, true));

            on_negotiation_deadline(generation);
            let message = read_state(|voice| voice.failure.as_ref().map(|f| f.message.clone()))
                .expect("a timed-out session must say why");
            assert!(
                message.contains("never connected"),
                "an admitted session blames the media route, got {message:?}"
            );
        });
    }

    #[test]
    fn the_deadline_does_not_fire_for_a_session_that_already_connected() {
        with_voice(|state| {
            start_for(&state, "a");
            let generation = force_live();
            on_negotiation_deadline(generation);
            assert!(is_engaged(), "a live session is not a stalled one");
            assert_eq!(fake().calls().stops, 0);
        });
    }

    /// Ownership follows the runtime, never the display phase. A terminal
    /// phase with a live runtime must still release on teardown.
    #[test]
    fn teardown_releases_media_even_when_the_phase_is_already_terminal() {
        with_voice(|state| {
            start_for(&state, "a");
            let generation = read_state(|voice| voice.generation);
            // Force the display into a terminal phase while the runtime lives.
            update_state(|voice| {
                voice.fail(
                    generation,
                    VoiceFailure {
                        stage: VoiceStage::Server,
                        message: "forced".to_owned(),
                        retryable: false,
                    },
                )
            });
            assert!(!is_engaged(), "the phase is terminal");
            assert!(runtime_is_live(), "but the microphone is not");

            enforce_target(&state, Some(&agent_ref("b")));
            assert_eq!(
                fake().calls().stops,
                1,
                "the guard must release media it can still see, whatever the phase says"
            );
            assert!(!runtime_is_live());
        });
    }

    #[test]
    fn starting_again_releases_any_runtime_the_previous_session_left_behind() {
        with_voice(|state| {
            start_for(&state, "a");
            let first = fake();
            let generation = read_state(|voice| voice.generation);
            update_state(|voice| {
                voice.fail(
                    generation,
                    VoiceFailure {
                        stage: VoiceStage::Server,
                        message: "forced".to_owned(),
                        retryable: false,
                    },
                )
            });
            assert!(runtime_is_live());

            start_for(&state, "a");
            assert_eq!(
                first.calls().stops,
                1,
                "a new session must never orphan the previous one's microphone"
            );
            assert!(is_engaged());
        });
    }

    /// Protocol 45 carries what was actually said. Dropping it would leave the
    /// desktop showing state labels while the host had real words for it.
    #[test]
    fn captions_and_typed_transcripts_from_the_host_are_shown() {
        with_voice(|state| {
            start_for(&state, "a");
            force_live();
            let session = read_state(|voice| voice.session.clone()).expect("session");

            assert!(handle_inbound(
                HOST,
                &transcript_frame(
                    &session,
                    protocol::VoiceSessionState::Listening,
                    protocol::VoiceTranscriptSpeaker::User,
                    "what is failing in the build",
                    true,
                )
            ));
            assert_eq!(
                read_state(|voice| voice.caption.clone()),
                Some("what is failing in the build".to_owned())
            );
            let lines = read_state(|voice| voice.transcript.clone());
            assert_eq!(lines.len(), 1);
            assert_eq!(lines[0].speaker, session::TranscriptSpeaker::User);
            assert!(lines[0].is_final);

            assert!(handle_inbound(
                HOST,
                &transcript_frame(
                    &session,
                    protocol::VoiceSessionState::Speaking,
                    protocol::VoiceTranscriptSpeaker::Assistant,
                    "the linker step",
                    true,
                )
            ));
            let lines = read_state(|voice| voice.transcript.clone());
            assert_eq!(lines.len(), 2, "both speakers are kept, in order");
            assert_eq!(lines[1].speaker, session::TranscriptSpeaker::Agent);
            assert_eq!(
                read_state(|voice| voice.caption.clone()),
                Some("the linker step".to_owned())
            );
        });
    }

    /// A partial is a revision of the same utterance, not a new one.
    #[test]
    fn a_partial_transcript_is_replaced_rather_than_appended() {
        with_voice(|state| {
            start_for(&state, "a");
            force_live();
            let session = read_state(|voice| voice.session.clone()).expect("session");

            for text in ["the", "the linker", "the linker step"] {
                assert!(handle_inbound(
                    HOST,
                    &transcript_frame(
                        &session,
                        protocol::VoiceSessionState::Speaking,
                        protocol::VoiceTranscriptSpeaker::Assistant,
                        text,
                        false,
                    )
                ));
            }
            let lines = read_state(|voice| voice.transcript.clone());
            assert_eq!(lines.len(), 1, "one utterance being revised, not three");
            assert_eq!(lines[0].text, "the linker step");

            // Finalising it, then starting a new one, does append.
            assert!(handle_inbound(
                HOST,
                &transcript_frame(
                    &session,
                    protocol::VoiceSessionState::Speaking,
                    protocol::VoiceTranscriptSpeaker::Assistant,
                    "the linker step",
                    true,
                )
            ));
            assert!(handle_inbound(
                HOST,
                &transcript_frame(
                    &session,
                    protocol::VoiceSessionState::Listening,
                    protocol::VoiceTranscriptSpeaker::User,
                    "show me",
                    true,
                )
            ));
            assert_eq!(read_state(|voice| voice.transcript.len()), 2);
        });
    }

    /// The host bounds transcript text; the client bounds it again so a server
    /// that stops clamping cannot produce an unbounded row.
    #[test]
    fn an_oversized_transcript_line_is_bounded_for_display() {
        let line = session::TranscriptLine::bounded(
            session::TranscriptSpeaker::Agent,
            "x".repeat(session::MAX_TRANSCRIPT_LINE_BYTES * 4),
            true,
        );
        assert!(line.text.len() <= session::MAX_TRANSCRIPT_LINE_BYTES + 4);
        assert!(
            line.text.ends_with('…'),
            "a truncated line must not read as a complete utterance"
        );

        // Multi-byte text is cut on a character boundary, not mid-codepoint.
        let line = session::TranscriptLine::bounded(
            session::TranscriptSpeaker::User,
            "é".repeat(session::MAX_TRANSCRIPT_LINE_BYTES),
            true,
        );
        assert!(line.text.chars().count() > 1);
    }

    /// The displayed transcript is bounded; the durable record is the chat
    /// underneath.
    #[test]
    fn only_the_most_recent_transcript_lines_are_kept() {
        with_voice(|state| {
            start_for(&state, "a");
            force_live();
            let session = read_state(|voice| voice.session.clone()).expect("session");
            for index in 0..(session::MAX_TRANSCRIPT_LINES + 3) {
                assert!(handle_inbound(
                    HOST,
                    &transcript_frame(
                        &session,
                        protocol::VoiceSessionState::Speaking,
                        protocol::VoiceTranscriptSpeaker::Assistant,
                        &format!("line {index}"),
                        true,
                    )
                ));
            }
            let lines = read_state(|voice| voice.transcript.clone());
            assert_eq!(lines.len(), session::MAX_TRANSCRIPT_LINES);
            assert_eq!(
                lines.last().map(|line| line.text.clone()),
                Some(format!("line {}", session::MAX_TRANSCRIPT_LINES + 2))
            );
        });
    }

    /// The server is known to send state changes carrying no caption while an
    /// utterance is still on screen. Clearing on those would blank the line.
    #[test]
    fn a_state_change_without_a_caption_leaves_the_last_one_visible() {
        with_voice(|state| {
            start_for(&state, "a");
            force_live();
            let session = read_state(|voice| voice.session.clone()).expect("session");

            assert!(handle_inbound(
                HOST,
                &transcript_frame(
                    &session,
                    protocol::VoiceSessionState::Speaking,
                    protocol::VoiceTranscriptSpeaker::Assistant,
                    "checking the build",
                    false,
                )
            ));
            assert!(handle_inbound(
                HOST,
                &state_frame(&session, protocol::VoiceSessionState::AgentWorking)
            ));
            assert_eq!(
                read_state(|voice| voice.caption.clone()),
                Some("checking the build".to_owned()),
                "a captionless state change means 'nothing new', not 'say nothing'"
            );
        });
    }

    #[test]
    fn a_new_session_starts_with_no_caption_or_transcript() {
        with_voice(|state| {
            start_for(&state, "a");
            force_live();
            let session = read_state(|voice| voice.session.clone()).expect("session");
            assert!(handle_inbound(
                HOST,
                &transcript_frame(
                    &session,
                    protocol::VoiceSessionState::Speaking,
                    protocol::VoiceTranscriptSpeaker::Assistant,
                    "old words",
                    true,
                )
            ));
            end(VoiceEndReason::UserRequested);
            dismiss();
            start_for(&state, "a");
            assert_eq!(read_state(|voice| voice.caption.clone()), None);
            assert!(read_state(|voice| voice.transcript.is_empty()));
        });
    }

    /// A route that never recovers may also never reach `failed`, and the
    /// negotiation deadline has already stood down by then.
    #[test]
    fn a_connection_that_stays_down_releases_the_microphone() {
        with_voice(|state| {
            start_for(&state, "a");
            let generation = force_live();
            let platform = fake();

            platform.emit(MediaEvent::ConnectionUnstable);
            assert!(is_engaged(), "instability alone is not terminal");
            let epoch = RUNTIME
                .with(|slot| {
                    slot.borrow()
                        .as_ref()
                        .map(|runtime| runtime.disconnect_epoch)
                })
                .expect("runtime");

            on_disconnect_grace_expired(generation, epoch);

            assert!(
                !is_engaged(),
                "an unrecovered route must not hold the microphone"
            );
            assert!(!runtime_is_live());
            assert_eq!(platform.calls().stops, 1);
            let message = read_state(|voice| voice.failure.as_ref().map(|f| f.message.clone()))
                .expect("the user must be told why");
            assert!(message.contains("did not recover"), "{message}");
        });
    }

    #[test]
    fn recovery_stands_the_disconnection_deadline_down() {
        with_voice(|state| {
            start_for(&state, "a");
            let generation = force_live();
            let platform = fake();

            platform.emit(MediaEvent::ConnectionUnstable);
            let stale_epoch = RUNTIME
                .with(|slot| {
                    slot.borrow()
                        .as_ref()
                        .map(|runtime| runtime.disconnect_epoch)
                })
                .expect("runtime");
            platform.emit(MediaEvent::Connected);
            assert!(
                read_state(|voice| voice.warning.is_none()),
                "recovery clears the warning"
            );

            on_disconnect_grace_expired(generation, stale_epoch);
            assert!(
                is_engaged(),
                "a timer armed before recovery must not end the recovered session"
            );
            assert_eq!(platform.calls().stops, 0);
        });
    }

    /// Drop, recover, drop again: the first timer must not end the session
    /// that the second disconnection is still waiting on.
    #[test]
    fn a_stale_disconnection_epoch_cannot_end_a_later_outage() {
        with_voice(|state| {
            start_for(&state, "a");
            let generation = force_live();
            let platform = fake();

            platform.emit(MediaEvent::ConnectionUnstable);
            let first_epoch = RUNTIME
                .with(|slot| {
                    slot.borrow()
                        .as_ref()
                        .map(|runtime| runtime.disconnect_epoch)
                })
                .expect("runtime");
            platform.emit(MediaEvent::Connected);
            platform.emit(MediaEvent::ConnectionUnstable);
            let second_epoch = RUNTIME
                .with(|slot| {
                    slot.borrow()
                        .as_ref()
                        .map(|runtime| runtime.disconnect_epoch)
                })
                .expect("runtime");
            assert_ne!(first_epoch, second_epoch);

            on_disconnect_grace_expired(generation, first_epoch);
            assert!(is_engaged(), "the superseded epoch is inert");

            on_disconnect_grace_expired(generation, second_epoch);
            assert!(!is_engaged(), "the live epoch still bounds the outage");
        });
    }

    /// Batches must not overlap: `addIceCandidate` is order-sensitive.
    #[test]
    fn remote_candidate_batches_drain_in_arrival_order_through_one_pump() {
        with_voice(|state| {
            start_for(&state, "a");
            let session = read_state(|voice| voice.session.clone()).expect("session");

            let batch = |names: &[&str]| {
                Envelope::from_payload(
                    session.stream.clone(),
                    FrameKind::VoiceIceCandidate,
                    0,
                    &protocol::VoiceIceCandidatePayload {
                        session_id: protocol::VoiceSessionId(session.session_id.clone()),
                        candidates: names
                            .iter()
                            .map(|name| protocol::VoiceIceCandidate {
                                candidate: (*name).to_owned(),
                                sdp_mid: Some("0".to_owned()),
                                sdp_m_line_index: Some(0),
                            })
                            .collect(),
                    },
                )
                .expect("encode candidates")
            };

            assert!(handle_inbound(HOST, &batch(&["a1", "a2"])));
            assert!(handle_inbound(HOST, &batch(&["b1"])));
            assert!(
                fake().calls().remote_candidates.is_empty(),
                "nothing is applied before the answer"
            );

            // The answer lands; everything drains once, in order.
            let answer = Envelope::from_payload(
                session.stream.clone(),
                FrameKind::VoiceAnswer,
                0,
                &protocol::VoiceAnswerPayload {
                    session_id: protocol::VoiceSessionId(session.session_id.clone()),
                    sdp: "v=0\r\n".to_owned(),
                },
            )
            .expect("encode answer");
            assert!(handle_inbound(HOST, &answer));
            pump_async_for_tests();

            let applied: Vec<String> = fake()
                .calls()
                .remote_candidates
                .into_iter()
                .map(|candidate| candidate.candidate)
                .collect();
            assert_eq!(
                applied,
                vec!["a1", "a2", "b1"],
                "arrival order is preserved"
            );

            // A batch arriving after the drain finished still goes through the
            // same pump rather than starting a competing one.
            assert!(handle_inbound(HOST, &batch(&["c1"])));
            pump_async_for_tests();
            let applied: Vec<String> = fake()
                .calls()
                .remote_candidates
                .into_iter()
                .map(|candidate| candidate.candidate)
                .collect();
            assert_eq!(applied, vec!["a1", "a2", "b1", "c1"]);
        });
    }

    /// A refused microphone is the most common way a session fails, and it has
    /// its own end reason so the strip can say "access was refused" rather
    /// than "audio stopped working". Nothing exercised that branch before.
    #[test]
    fn a_refused_microphone_ends_the_session_as_a_permission_problem() {
        with_scripted_voice(
            FakeMediaScript {
                start_error: Some(MediaError::microphone("Permission dismissed by the user")),
                ..FakeMediaScript::default()
            },
            |state| {
                start_for(&state, "a");

                assert!(!is_engaged());
                assert!(!runtime_is_live());
                assert_eq!(
                    read_state(|voice| voice.ended_reason),
                    Some(VoiceEndReason::PermissionDenied),
                    "a microphone-stage failure is a permission problem, not a media one"
                );
                let message = read_state(|voice| voice.failure.as_ref().map(|f| f.message.clone()))
                    .expect("the browser's own reason must reach the user");
                assert_eq!(message, "Permission dismissed by the user");
                assert_eq!(
                    read_state(|voice| voice.failure.as_ref().map(|f| f.stage)),
                    Some(VoiceStage::Microphone)
                );
            },
        );
    }

    /// A negotiation-stage failure is *not* a permission problem, and must not
    /// be reported as one.
    #[test]
    fn a_rejected_answer_ends_the_session_as_a_media_problem() {
        with_scripted_voice(
            FakeMediaScript {
                answer_error: Some(MediaError::negotiation("the host rejected the answer")),
                ..FakeMediaScript::default()
            },
            |state| {
                start_for(&state, "a");
                let session = read_state(|voice| voice.session.clone()).expect("session");
                let answer = Envelope::from_payload(
                    session.stream.clone(),
                    FrameKind::VoiceAnswer,
                    0,
                    &protocol::VoiceAnswerPayload {
                        session_id: protocol::VoiceSessionId(session.session_id.clone()),
                        sdp: "v=0\r\n".to_owned(),
                    },
                )
                .expect("encode answer");

                assert!(handle_inbound(HOST, &answer));
                pump_async_for_tests();

                assert!(!is_engaged());
                assert!(!runtime_is_live());
                assert_eq!(
                    read_state(|voice| voice.ended_reason),
                    Some(VoiceEndReason::MediaFailed)
                );
                assert_eq!(
                    read_state(|voice| voice.failure.as_ref().map(|f| f.stage)),
                    Some(VoiceStage::Negotiation),
                    "a rejected answer is a setup failure, not a refused microphone"
                );
            },
        );
    }

    /// Autoplay refusal is the platform-driven half of the "Tap to hear" path;
    /// only the user-driven half (`silence_output`) was covered before.
    #[test]
    fn blocked_playback_offers_recovery_and_attaching_the_track_clears_it() {
        with_voice(|state| {
            start_for(&state, "a");
            force_live();
            let platform = fake();

            platform.emit(MediaEvent::PlaybackBlocked("play() was refused".to_owned()));
            assert!(
                read_state(|voice| voice.playback_blocked),
                "a refused play() must surface the recovery control"
            );
            assert!(is_engaged(), "blocked output is not a dead session");

            platform.emit(MediaEvent::RemoteTrackAttached);
            assert!(
                !read_state(|voice| voice.playback_blocked),
                "a freshly attached remote track is audible again"
            );
        });
    }
}
