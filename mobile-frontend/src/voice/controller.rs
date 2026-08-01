use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::Duration;

use leptos::prelude::*;
use send_wrapper::SendWrapper;

use super::media::{BrowserVoiceMedia, MediaCallbacks, VoiceMediaFactory, VoiceMediaSession};
use super::model::{IceBatcher, IceCandidate, VoiceModel, VoicePhase, VoiceSession, VoiceTarget};
use crate::send::send_frame;
use crate::state::{AppState, ConnectionStatus};
use protocol::{
    FrameKind, VoiceAnswerPayload, VoiceAudioCodec, VoiceClientCapabilities, VoiceErrorCode,
    VoiceErrorPayload, VoiceIceCandidatePayload, VoiceIceCandidatesCompletePayload,
    VoiceOfferPayload, VoiceSessionId, VoiceStartPayload, VoiceStatePayload, VoiceStopPayload,
    VoiceStopReason,
};

pub type SignalFuture = Pin<Box<dyn Future<Output = Result<(), String>>>>;

const NEGOTIATION_TIMEOUT: Duration = Duration::from_secs(20);
const DISCONNECTED_GRACE: Duration = Duration::from_secs(8);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VoiceEntryAvailability {
    Available,
    Unavailable(&'static str),
}

/// The only voice wire boundary. Production implements this with typed protocol
/// frames over the existing MQTT connection; tests inject a recorder. Audio is
/// deliberately absent from every method.
pub trait VoiceSignaling {
    fn start(&self, target: VoiceTarget, session: VoiceSession, offer_sdp: String) -> SignalFuture;
    fn ice(
        &self,
        target: VoiceTarget,
        session: VoiceSession,
        candidates: Vec<IceCandidate>,
        complete: bool,
    ) -> SignalFuture;
    fn stop(
        &self,
        target: VoiceTarget,
        session: VoiceSession,
        reason: &'static str,
    ) -> SignalFuture;
}

#[derive(Default)]
struct ProtocolVoiceSignaling {
    send_lock: Rc<tokio::sync::Mutex<()>>,
}

impl VoiceSignaling for ProtocolVoiceSignaling {
    fn start(&self, target: VoiceTarget, session: VoiceSession, offer_sdp: String) -> SignalFuture {
        let lock = self.send_lock.clone();
        Box::pin(async move {
            let _guard = lock.lock().await;
            let session_id = VoiceSessionId(session.id.clone());
            send_frame(
                &target.local_host_id,
                session.stream.clone(),
                FrameKind::VoiceStart,
                &VoiceStartPayload {
                    session_id: session_id.clone(),
                    target: protocol::VoiceTarget {
                        agent_id: target.agent_id,
                        instance_stream: target.instance_stream,
                    },
                    capabilities: VoiceClientCapabilities {
                        audio_track: true,
                        codecs: vec![VoiceAudioCodec::Opus],
                        echo_cancellation_requested: true,
                    },
                },
            )
            .await
            .map_err(|error| error.to_string())?;
            send_frame(
                &target.local_host_id,
                session.stream,
                FrameKind::VoiceOffer,
                &VoiceOfferPayload {
                    session_id,
                    sdp: offer_sdp,
                },
            )
            .await
            .map(|_| ())
            .map_err(|error| error.to_string())
        })
    }

    fn ice(
        &self,
        target: VoiceTarget,
        session: VoiceSession,
        candidates: Vec<IceCandidate>,
        complete: bool,
    ) -> SignalFuture {
        let lock = self.send_lock.clone();
        Box::pin(async move {
            let _guard = lock.lock().await;
            let session_id = VoiceSessionId(session.id.clone());
            if !candidates.is_empty() {
                let candidates = candidates
                    .into_iter()
                    .map(|candidate| protocol::VoiceIceCandidate {
                        candidate: candidate.candidate,
                        sdp_mid: candidate.sdp_mid,
                        sdp_m_line_index: candidate.sdp_m_line_index,
                    })
                    .collect();
                send_frame(
                    &target.local_host_id,
                    session.stream.clone(),
                    FrameKind::VoiceIceCandidate,
                    &VoiceIceCandidatePayload {
                        session_id: session_id.clone(),
                        candidates,
                    },
                )
                .await
                .map_err(|error| error.to_string())?;
            }
            if complete {
                send_frame(
                    &target.local_host_id,
                    session.stream,
                    FrameKind::VoiceIceCandidatesComplete,
                    &VoiceIceCandidatesCompletePayload { session_id },
                )
                .await
                .map_err(|error| error.to_string())?;
            }
            Ok(())
        })
    }

    fn stop(
        &self,
        target: VoiceTarget,
        session: VoiceSession,
        reason: &'static str,
    ) -> SignalFuture {
        let lock = self.send_lock.clone();
        Box::pin(async move {
            let _guard = lock.lock().await;
            send_frame(
                &target.local_host_id,
                session.stream,
                FrameKind::VoiceStop,
                &VoiceStopPayload {
                    session_id: VoiceSessionId(session.id),
                    reason: stop_reason(reason),
                },
            )
            .await
            .map(|_| ())
            .map_err(|error| error.to_string())
        })
    }
}

fn stop_reason(reason: &str) -> VoiceStopReason {
    match reason {
        "focus-changed" | "replaced" => VoiceStopReason::FocusChanged,
        "backgrounded" | "page-closed" => VoiceStopReason::ClientBackgrounded,
        "media-failed" => VoiceStopReason::MediaFailed,
        _ => VoiceStopReason::UserExited,
    }
}

struct Runtime {
    generation: u64,
    target: Option<VoiceTarget>,
    session: Option<VoiceSession>,
    media: Option<Rc<dyn VoiceMediaSession>>,
    ice: IceBatcher,
    remote_ice: IceBatcher,
    answer_applied: bool,
    remote_flush_active: bool,
    signaling_state: SignalingState,
    connection_epoch: u64,
    negotiation_cancel: Option<tokio::sync::oneshot::Sender<()>>,
    disconnected_cancel: Option<tokio::sync::oneshot::Sender<()>>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum SignalingState {
    #[default]
    NotStarted,
    Starting,
    Started,
}

impl Default for Runtime {
    fn default() -> Self {
        Self {
            generation: 0,
            target: None,
            session: None,
            media: None,
            ice: IceBatcher::default(),
            remote_ice: IceBatcher::default(),
            answer_applied: false,
            remote_flush_active: false,
            signaling_state: SignalingState::NotStarted,
            connection_epoch: 0,
            negotiation_cancel: None,
            disconnected_cancel: None,
        }
    }
}

struct Inner {
    model: RwSignal<VoiceModel>,
    runtime: RefCell<Runtime>,
    media: Rc<dyn VoiceMediaFactory>,
    signaling: Rc<dyn VoiceSignaling>,
}

// Leptos requires context and reactive captures to carry Send + Sync marker
// bounds even in its single-threaded CSR runtime. Browser media and callbacks
// are thread-affine, so the wrapper enforces same-thread access at runtime
// instead of falsely making those values independently thread-safe.
#[derive(Clone)]
pub struct VoiceController(Arc<SendWrapper<Rc<Inner>>>);

impl Default for VoiceController {
    fn default() -> Self {
        Self::new()
    }
}

impl VoiceController {
    pub fn new() -> Self {
        Self::with_dependencies(
            Rc::new(BrowserVoiceMedia),
            Rc::new(ProtocolVoiceSignaling::default()),
        )
    }

    pub fn with_dependencies(
        media: Rc<dyn VoiceMediaFactory>,
        signaling: Rc<dyn VoiceSignaling>,
    ) -> Self {
        Self::from_inner(Rc::new(Inner {
            model: RwSignal::new(VoiceModel::Idle),
            runtime: RefCell::new(Runtime::default()),
            media,
            signaling,
        }))
    }

    fn from_inner(inner: Rc<Inner>) -> Self {
        Self(Arc::new(SendWrapper::new(inner)))
    }

    fn local_inner(&self) -> &Rc<Inner> {
        &self.0
    }

    pub fn model(&self) -> RwSignal<VoiceModel> {
        self.0.model
    }

    pub fn target_for_active_agent(state: &AppState) -> Result<VoiceTarget, String> {
        let active = state
            .active_agent
            .get_untracked()
            .ok_or_else(|| "Open an agent before starting voice.".to_owned())?;
        if !matches!(
            state
                .connection_statuses
                .with_untracked(|statuses| statuses.get(&active.local_host_id).cloned()),
            Some(ConnectionStatus::Connected)
        ) {
            return Err("Voice needs a connected Tyde host.".to_owned());
        }
        if let VoiceEntryAvailability::Unavailable(reason) =
            Self::entry_availability_for_active_agent(state)
        {
            return Err(reason.to_owned());
        }
        state
            .agents
            .with_untracked(|agents| {
                agents
                    .iter()
                    .find(|agent| {
                        agent.local_host_id == active.local_host_id
                            && agent.agent_id == active.agent_id
                    })
                    .map(|agent| VoiceTarget {
                        local_host_id: agent.local_host_id.clone(),
                        agent_id: agent.agent_id.clone(),
                        instance_stream: agent.instance_stream.clone(),
                        agent_name: agent.name.clone(),
                    })
            })
            .ok_or_else(|| "The focused agent is no longer available.".to_owned())
    }

    pub fn entry_availability_for_active_agent(state: &AppState) -> VoiceEntryAvailability {
        let Some(active) = state.active_agent.get_untracked() else {
            return VoiceEntryAvailability::Unavailable("Open an agent to use voice.");
        };
        if !matches!(
            state
                .connection_statuses
                .with_untracked(|statuses| statuses.get(&active.local_host_id).cloned()),
            Some(ConnectionStatus::Connected)
        ) {
            return VoiceEntryAvailability::Unavailable("Reconnect the host to use voice.");
        }
        let availability = state.host_settings_by_host.with_untracked(|settings| {
            settings
                .get(&active.local_host_id)
                .map(|settings| settings.voice.availability.clone())
        });
        match availability {
            Some(protocol::VoiceAvailability::Available { .. }) => {
                VoiceEntryAvailability::Available
            }
            Some(protocol::VoiceAvailability::Unavailable {
                reason: protocol::VoiceUnavailableReason::NoReachableCandidate,
            }) => VoiceEntryAvailability::Unavailable("Same LAN or VPN required for voice."),
            Some(protocol::VoiceAvailability::Unavailable {
                reason: protocol::VoiceUnavailableReason::NotEnabled,
            }) => VoiceEntryAvailability::Unavailable("Enable voice in host settings."),
            Some(protocol::VoiceAvailability::Unavailable {
                reason: protocol::VoiceUnavailableReason::RegionNotConfigured,
            }) => VoiceEntryAvailability::Unavailable("Set the AWS region on the host."),
            Some(protocol::VoiceAvailability::Unavailable {
                reason: protocol::VoiceUnavailableReason::ServerAdapterUnavailable,
            }) => VoiceEntryAvailability::Unavailable("Voice is unavailable in this host build."),
            None => VoiceEntryAvailability::Unavailable("Checking voice availability…"),
        }
    }

    pub fn start_for_active_agent(&self, state: &AppState) {
        match Self::target_for_active_agent(state) {
            Ok(target) => self.start(target),
            Err(message) => {
                self.end("focus-changed");
                self.fail_without_target(message);
            }
        }
    }

    pub fn start(&self, target: VoiceTarget) {
        self.end("replaced");
        let generation = {
            let mut runtime = self.0.runtime.borrow_mut();
            runtime.generation = runtime.generation.wrapping_add(1);
            runtime.target = Some(target.clone());
            runtime.ice = IceBatcher::default();
            runtime.remote_ice = IceBatcher::default();
            runtime.answer_applied = false;
            runtime.remote_flush_active = false;
            runtime.signaling_state = SignalingState::NotStarted;
            runtime.connection_epoch = runtime.connection_epoch.wrapping_add(1);
            runtime.generation
        };
        self.0.model.set(VoiceModel::Live {
            generation,
            target: target.clone(),
            session: None,
            phase: VoicePhase::RequestingMicrophone,
            muted: false,
            processing: Default::default(),
            playback_blocked: false,
            caption: None,
            transcript: None,
        });
        self.arm_negotiation_timeout(generation);

        // Mobile prepares browser media before sequence-zero VoiceStart.
        // Desktop may announce VoiceStart before its permission prompt; both
        // orders are valid, but mobile must preserve media-before-offer so the
        // browser gathers the real audio-track ICE state for this session.
        let weak = Rc::downgrade(self.local_inner());
        let callbacks = callbacks(weak.clone(), generation);
        let media = self.0.media.clone();
        spawn_voice(async move {
            match media.prepare(callbacks).await {
                Ok(prepared) => {
                    let Some(inner) = weak.upgrade() else { return };
                    let controller = VoiceController::from_inner(inner);
                    if !controller.is_current(generation) {
                        prepared.session.close_now();
                        return;
                    }
                    {
                        let mut runtime = controller.0.runtime.borrow_mut();
                        runtime.media = Some(prepared.session);
                    }
                    controller.0.model.update(|model| {
                        if let VoiceModel::Live {
                            generation: current,
                            phase,
                            processing,
                            ..
                        } = model
                            && *current == generation
                        {
                            *phase = VoicePhase::Connecting;
                            *processing = prepared.processing;
                        }
                    });
                    let id = uuid::Uuid::new_v4().to_string();
                    let session = VoiceSession {
                        stream: protocol::StreamPath(format!("/voice/{id}")),
                        id,
                    };
                    {
                        let mut runtime = controller.0.runtime.borrow_mut();
                        runtime.session = Some(session.clone());
                        runtime.signaling_state = SignalingState::Starting;
                    }
                    controller.0.model.update(|model| {
                        if let VoiceModel::Live {
                            generation: current,
                            session: current_session,
                            ..
                        } = model
                            && *current == generation
                        {
                            *current_session = Some(session.clone());
                        }
                    });
                    let signaling = controller.0.signaling.clone();
                    let result = signaling.start(target, session, prepared.offer_sdp).await;
                    if let Err(error) = result {
                        controller.fail_current(generation, safe_signaling_error(error));
                    } else if controller.is_current(generation) {
                        controller.0.runtime.borrow_mut().signaling_state = SignalingState::Started;
                        controller.flush_ice(generation);
                    }
                }
                Err(error) => {
                    if let Some(inner) = weak.upgrade() {
                        VoiceController::from_inner(inner)
                            .fail_current(generation, safe_media_error(error));
                    }
                }
            }
        });
    }

    /// Applies an SDP answer on the dedicated voice stream. The immutable
    /// target and local generation must both still match.
    pub fn opened(
        &self,
        generation: u64,
        target: &VoiceTarget,
        session: VoiceSession,
        answer_sdp: String,
    ) {
        if !self.target_matches(generation, target) {
            return;
        }
        let media = {
            let mut runtime = self.0.runtime.borrow_mut();
            runtime.session = Some(session.clone());
            runtime.media.clone()
        };
        self.0.model.update(|model| {
            if let VoiceModel::Live {
                generation: current,
                session: current_session,
                ..
            } = model
                && *current == generation
            {
                *current_session = Some(session.clone());
            }
        });
        let Some(media) = media else {
            self.fail_current(
                generation,
                "Voice media ended before the host answered.".to_owned(),
            );
            return;
        };
        let weak = Rc::downgrade(self.local_inner());
        spawn_voice(async move {
            match media.apply_answer(answer_sdp).await {
                Ok(()) => {
                    if let Some(inner) = weak.upgrade() {
                        let controller = VoiceController::from_inner(inner);
                        if controller.is_current(generation) {
                            controller.0.runtime.borrow_mut().answer_applied = true;
                            controller.flush_remote_ice(generation);
                        }
                    }
                }
                Err(error) => {
                    if let Some(inner) = weak.upgrade() {
                        VoiceController::from_inner(inner)
                            .fail_current(generation, safe_media_error(error));
                    }
                }
            }
        });
        self.flush_ice(generation);
    }

    pub fn apply_ready(
        &self,
        host: &crate::state::LocalHostId,
        stream: &protocol::StreamPath,
        payload: protocol::VoiceReadyPayload,
    ) {
        let runtime = self.0.runtime.borrow();
        let Some(target) = runtime.target.as_ref() else {
            return;
        };
        let Some(session) = runtime.session.as_ref() else {
            return;
        };
        if &target.local_host_id != host
            || &session.stream != stream
            || session.id != payload.session_id.0
        {
            return;
        }
        let generation = runtime.generation;
        let target_matches = target.agent_id == payload.target.agent_id
            && target.instance_stream == payload.target.instance_stream;
        drop(runtime);
        if !target_matches {
            self.fail_current(
                generation,
                "The host answered voice for a different agent. Voice was ended safely.".to_owned(),
            );
            return;
        }
        self.set_phase(generation, VoicePhase::Connecting);
    }

    pub fn apply_answer(
        &self,
        host: &crate::state::LocalHostId,
        stream: &protocol::StreamPath,
        payload: VoiceAnswerPayload,
    ) {
        let runtime = self.0.runtime.borrow();
        let Some(target) = runtime.target.clone() else {
            return;
        };
        let Some(session) = runtime.session.clone() else {
            return;
        };
        if &target.local_host_id != host
            || &session.stream != stream
            || session.id != payload.session_id.0
        {
            return;
        }
        let generation = runtime.generation;
        drop(runtime);
        self.opened(generation, &target, session, payload.sdp);
    }

    pub fn apply_remote_ice(
        &self,
        host: &crate::state::LocalHostId,
        stream: &protocol::StreamPath,
        payload: VoiceIceCandidatePayload,
    ) {
        let Some(generation) = self.matching_generation(host, stream, &payload.session_id) else {
            return;
        };
        for candidate in payload.candidates {
            let queued = self.0.runtime.borrow_mut().remote_ice.push(IceCandidate {
                candidate: candidate.candidate,
                sdp_mid: candidate.sdp_mid,
                sdp_m_line_index: candidate.sdp_m_line_index,
                username_fragment: None,
            });
            if queued.is_err() {
                self.fail_current(
                    generation,
                    "Voice negotiation exceeded the safe signaling limit.".to_owned(),
                );
                return;
            }
        }
        self.flush_remote_ice(generation);
    }

    pub fn apply_remote_ice_complete(
        &self,
        host: &crate::state::LocalHostId,
        stream: &protocol::StreamPath,
        payload: VoiceIceCandidatesCompletePayload,
    ) {
        let Some(generation) = self.matching_generation(host, stream, &payload.session_id) else {
            return;
        };
        self.0.runtime.borrow_mut().remote_ice.mark_complete();
        self.flush_remote_ice(generation);
    }

    pub fn apply_state(
        &self,
        host: &crate::state::LocalHostId,
        stream: &protocol::StreamPath,
        payload: VoiceStatePayload,
    ) {
        let Some(generation) = self.matching_generation(host, stream, &payload.session_id) else {
            return;
        };
        let progress = payload.progress.map(|progress| progress.source_kind);
        let phase = match payload.state {
            protocol::VoiceSessionState::Negotiating => VoicePhase::Connecting,
            protocol::VoiceSessionState::Connected | protocol::VoiceSessionState::Listening => {
                VoicePhase::Listening
            }
            protocol::VoiceSessionState::AgentWorking => VoicePhase::Working(progress),
            protocol::VoiceSessionState::Speaking => VoicePhase::Speaking,
            protocol::VoiceSessionState::Ending => VoicePhase::Stopping,
            protocol::VoiceSessionState::Ended => {
                self.finish_remote(generation);
                return;
            }
        };
        if !matches!(
            &phase,
            VoicePhase::RequestingMicrophone | VoicePhase::Connecting
        ) {
            self.cancel_negotiation_timeout();
        }
        let caption = payload.caption.map(|caption| bounded_voice_text(&caption));
        let transcript = payload.transcript.map(|mut transcript| {
            transcript.text = bounded_voice_text(&transcript.text);
            transcript
        });
        let mut resume_playback = false;
        self.0.model.update(|model| {
            if let VoiceModel::Live {
                generation: current,
                phase: current_phase,
                playback_blocked,
                caption: current_caption,
                transcript: current_transcript,
                ..
            } = model
                && *current == generation
            {
                resume_playback = phase == VoicePhase::Speaking
                    && *current_phase != VoicePhase::Speaking
                    && *playback_blocked;
                *current_phase = phase;
                *current_caption = caption;
                *current_transcript = transcript;
            }
        });
        if resume_playback {
            self.resume_playback();
        }
    }

    pub fn apply_error(
        &self,
        host: &crate::state::LocalHostId,
        stream: &protocol::StreamPath,
        payload: VoiceErrorPayload,
    ) {
        let Some(generation) = self.matching_generation(host, stream, &payload.session_id) else {
            return;
        };
        let message = match payload.code {
            VoiceErrorCode::MediaNegotiationFailed => {
                "Voice is unavailable: no direct route to this Tyde host. Connect both devices to the same LAN or VPN and try again."
            }
            VoiceErrorCode::NotAvailable | VoiceErrorCode::ProviderUnavailable => {
                "Voice is not configured on this Tyde host."
            }
            VoiceErrorCode::AgentUnavailable => {
                "The selected agent is no longer available for voice."
            }
            VoiceErrorCode::AlreadyActive => {
                "Another voice session is already active on this host."
            }
            _ => "The voice session ended because the host reported an error.",
        };
        if payload.fatal {
            self.fail_current(generation, message.to_owned());
        }
    }

    pub fn set_phase(&self, generation: u64, phase: VoicePhase) {
        self.0.model.update(|model| {
            if let VoiceModel::Live {
                generation: current,
                phase: current_phase,
                ..
            } = model
                && *current == generation
            {
                *current_phase = phase;
            }
        });
    }

    pub fn toggle_mute(&self) {
        let (media, muted) = {
            let mut next = None;
            self.0.model.update(|model| {
                if let VoiceModel::Live { muted, .. } = model {
                    *muted = !*muted;
                    next = Some(*muted);
                }
            });
            (self.0.runtime.borrow().media.clone(), next)
        };
        if let (Some(media), Some(muted)) = (media, muted) {
            media.set_muted(muted);
        }
    }

    pub fn interrupt(&self) {
        let (media, generation) = {
            let runtime = self.0.runtime.borrow();
            (runtime.media.clone(), runtime.generation)
        };
        let Some(media) = media else { return };
        media.interrupt_playback();
        self.0.model.update(|model| {
            if let VoiceModel::Live {
                generation: current,
                playback_blocked,
                ..
            } = model
                && *current == generation
            {
                *playback_blocked = true;
            }
        });
    }

    pub fn resume_playback(&self) {
        let media = self.0.runtime.borrow().media.clone();
        let weak = Rc::downgrade(self.local_inner());
        let generation = self.0.runtime.borrow().generation;
        if let Some(media) = media {
            spawn_voice(async move {
                match media.resume_playback().await {
                    Ok(()) => {
                        if let Some(inner) = weak.upgrade() {
                            inner.model.update(|model| {
                                if let VoiceModel::Live {
                                    generation: current,
                                    playback_blocked,
                                    ..
                                } = model
                                    && *current == generation
                                {
                                    *playback_blocked = false;
                                }
                            });
                        }
                    }
                    Err(error) => log::warn!(
                        "voice playback remains blocked: {}",
                        safe_media_error(error)
                    ),
                }
            });
        }
    }

    /// Closes microphone, peer connection, and output synchronously before the
    /// best-effort MQTT stop frame. It never interrupts or closes the agent.
    pub fn end(&self, reason: &'static str) {
        let (target, session, media) = {
            let mut runtime = self.0.runtime.borrow_mut();
            runtime.generation = runtime.generation.wrapping_add(1);
            let result = (
                runtime.target.take(),
                runtime.session.take(),
                runtime.media.take(),
            );
            runtime.ice = IceBatcher::default();
            runtime.remote_ice = IceBatcher::default();
            runtime.answer_applied = false;
            runtime.remote_flush_active = false;
            runtime.signaling_state = SignalingState::NotStarted;
            runtime.connection_epoch = runtime.connection_epoch.wrapping_add(1);
            cancel_deadline(&mut runtime.negotiation_cancel);
            cancel_deadline(&mut runtime.disconnected_cancel);
            result
        };
        if let Some(media) = media {
            media.close_now();
        }
        self.0.model.set(VoiceModel::Idle);
        if let (Some(target), Some(session)) = (target, session) {
            let signaling = self.0.signaling.clone();
            spawn_voice(async move {
                if let Err(error) = signaling.stop(target, session, reason).await {
                    log::warn!(
                        "voice stop signaling failed: {}",
                        safe_signaling_error(error)
                    );
                }
            });
        }
    }

    pub fn terminate_if_target_changed(&self, active: Option<VoiceTarget>) {
        let current = self.0.runtime.borrow().target.clone();
        if current.is_some() && current != active {
            self.end("focus-changed");
            return;
        }
        let stale_failure = self.0.model.with_untracked(|model| {
            matches!(
                model,
                VoiceModel::Failed {
                    target: Some(target),
                    ..
                } if Some(target) != active.as_ref()
            )
        });
        if stale_failure {
            self.0.model.set(VoiceModel::Idle);
        }
    }

    fn local_ice(&self, generation: u64, candidate: Option<IceCandidate>) {
        if !self.is_current(generation) {
            return;
        }
        let should_flush = {
            let mut runtime = self.0.runtime.borrow_mut();
            match candidate {
                Some(candidate) => match runtime.ice.push(candidate) {
                    Ok(ready) => ready,
                    Err(_) => {
                        drop(runtime);
                        self.fail_current(
                            generation,
                            "Voice negotiation exceeded the safe signaling limit.".to_owned(),
                        );
                        return;
                    }
                },
                None => {
                    runtime.ice.mark_complete();
                    true
                }
            }
        };
        if should_flush {
            self.flush_ice(generation);
        }
    }

    fn arm_negotiation_timeout(&self, generation: u64) {
        let (cancel, canceled) = tokio::sync::oneshot::channel();
        {
            let mut runtime = self.0.runtime.borrow_mut();
            cancel_deadline(&mut runtime.negotiation_cancel);
            runtime.negotiation_cancel = Some(cancel);
        }
        let weak = Rc::downgrade(self.local_inner());
        spawn_voice(async move {
            tokio::select! {
                _ = sleep_voice(NEGOTIATION_TIMEOUT) => {
                    if let Some(inner) = weak.upgrade() {
                        VoiceController::from_inner(inner).expire_negotiation(generation);
                    }
                }
                _ = canceled => {}
            }
        });
    }

    fn cancel_negotiation_timeout(&self) {
        cancel_deadline(&mut self.0.runtime.borrow_mut().negotiation_cancel);
    }

    fn expire_negotiation(&self, generation: u64) {
        let still_negotiating = self.0.model.with_untracked(|model| {
            matches!(
                model,
                VoiceModel::Live {
                    generation: current,
                    phase: VoicePhase::RequestingMicrophone | VoicePhase::Connecting,
                    ..
                } if *current == generation
            )
        });
        if still_negotiating {
            self.fail_current(
                generation,
                "Voice is unavailable: no direct route to this Tyde host. Connect both devices to the same LAN or VPN and try again."
                    .to_owned(),
            );
        }
    }

    fn connection_interrupted(&self, generation: u64) {
        if !self.is_current(generation) {
            return;
        }
        let (epoch, canceled) = {
            let mut runtime = self.0.runtime.borrow_mut();
            runtime.connection_epoch = runtime.connection_epoch.wrapping_add(1);
            cancel_deadline(&mut runtime.disconnected_cancel);
            let (cancel, canceled) = tokio::sync::oneshot::channel();
            runtime.disconnected_cancel = Some(cancel);
            (runtime.connection_epoch, canceled)
        };
        let weak = Rc::downgrade(self.local_inner());
        spawn_voice(async move {
            tokio::select! {
                _ = sleep_voice(DISCONNECTED_GRACE) => {
                    if let Some(inner) = weak.upgrade() {
                        VoiceController::from_inner(inner).expire_disconnected(generation, epoch);
                    }
                }
                _ = canceled => {}
            }
        });
    }

    fn connection_recovered(&self, generation: u64) {
        if self.is_current(generation) {
            let mut runtime = self.0.runtime.borrow_mut();
            runtime.connection_epoch = runtime.connection_epoch.wrapping_add(1);
            cancel_deadline(&mut runtime.disconnected_cancel);
        }
    }

    fn expire_disconnected(&self, generation: u64, epoch: u64) {
        let expired = {
            let runtime = self.0.runtime.borrow();
            runtime.generation == generation
                && runtime.target.is_some()
                && runtime.connection_epoch == epoch
        };
        if expired {
            self.fail_current(
                generation,
                "Voice is unavailable: no direct route to this Tyde host. Connect both devices to the same LAN or VPN and try again."
                    .to_owned(),
            );
        }
    }

    fn flush_ice(&self, generation: u64) {
        loop {
            let next = {
                let mut runtime = self.0.runtime.borrow_mut();
                if runtime.generation != generation {
                    return;
                }
                if runtime.signaling_state != SignalingState::Started {
                    return;
                }
                let (Some(target), Some(session)) =
                    (runtime.target.clone(), runtime.session.clone())
                else {
                    return;
                };
                let candidates = runtime.ice.take_batch();
                let complete = runtime.ice.take_completion();
                if candidates.is_empty() && !complete {
                    return;
                }
                Some((target, session, candidates, complete))
            };
            let Some((target, session, candidates, complete)) = next else {
                return;
            };
            let signaling = self.0.signaling.clone();
            let weak = Rc::downgrade(self.local_inner());
            spawn_voice(async move {
                if let Err(error) = signaling.ice(target, session, candidates, complete).await
                    && let Some(inner) = weak.upgrade()
                {
                    VoiceController::from_inner(inner)
                        .fail_current(generation, safe_signaling_error(error));
                }
            });
            if complete {
                return;
            }
        }
    }

    fn flush_remote_ice(&self, generation: u64) {
        let next = {
            let mut runtime = self.0.runtime.borrow_mut();
            if runtime.generation != generation
                || !runtime.answer_applied
                || runtime.remote_flush_active
            {
                return;
            }
            let Some(media) = runtime.media.clone() else {
                return;
            };
            let candidates = runtime.remote_ice.take_batch();
            let complete = runtime.remote_ice.take_completion();
            if candidates.is_empty() && !complete {
                return;
            }
            runtime.remote_flush_active = true;
            (media, candidates, complete)
        };
        let (media, candidates, complete) = next;
        let weak = Rc::downgrade(self.local_inner());
        spawn_voice(async move {
            let mut result = Ok(());
            for candidate in candidates {
                if let Err(error) = media.add_remote_candidate(candidate).await {
                    result = Err(error);
                    break;
                }
            }
            if result.is_ok() && complete {
                result = media.finish_remote_candidates().await;
            }
            if let Some(inner) = weak.upgrade() {
                let controller = VoiceController::from_inner(inner);
                if let Err(error) = result {
                    controller.fail_current(generation, safe_media_error(error));
                } else if controller.is_current(generation) {
                    controller.0.runtime.borrow_mut().remote_flush_active = false;
                    if !complete {
                        controller.flush_remote_ice(generation);
                    }
                }
            }
        });
    }

    fn target_matches(&self, generation: u64, target: &VoiceTarget) -> bool {
        let runtime = self.0.runtime.borrow();
        runtime.generation == generation && runtime.target.as_ref() == Some(target)
    }

    fn matching_generation(
        &self,
        host: &crate::state::LocalHostId,
        stream: &protocol::StreamPath,
        session_id: &VoiceSessionId,
    ) -> Option<u64> {
        let runtime = self.0.runtime.borrow();
        let target = runtime.target.as_ref()?;
        let session = runtime.session.as_ref()?;
        (&target.local_host_id == host && &session.stream == stream && session.id == session_id.0)
            .then_some(runtime.generation)
    }

    fn is_current(&self, generation: u64) -> bool {
        self.0.runtime.borrow().generation == generation && self.0.runtime.borrow().target.is_some()
    }

    fn fail_without_target(&self, message: String) {
        let generation = {
            let mut runtime = self.0.runtime.borrow_mut();
            runtime.generation = runtime.generation.wrapping_add(1);
            runtime.generation
        };
        self.0.model.set(VoiceModel::Failed {
            generation,
            target: None,
            message,
        });
    }

    fn fail_current(&self, generation: u64, message: String) {
        if !self.is_current(generation) {
            return;
        }
        let (target, session, media) = {
            let mut runtime = self.0.runtime.borrow_mut();
            let resources = (
                runtime.target.take(),
                runtime.session.take(),
                runtime.media.take(),
            );
            runtime.ice = IceBatcher::default();
            runtime.remote_ice = IceBatcher::default();
            runtime.answer_applied = false;
            runtime.remote_flush_active = false;
            runtime.signaling_state = SignalingState::NotStarted;
            runtime.connection_epoch = runtime.connection_epoch.wrapping_add(1);
            cancel_deadline(&mut runtime.negotiation_cancel);
            cancel_deadline(&mut runtime.disconnected_cancel);
            resources
        };
        if let Some(media) = media {
            media.close_now();
        }
        if let (Some(target), Some(session)) = (target.clone(), session) {
            let signaling = self.0.signaling.clone();
            spawn_voice(async move {
                if let Err(error) = signaling.stop(target, session, "media-failed").await {
                    log::warn!(
                        "voice failure stop signaling failed: {}",
                        safe_signaling_error(error)
                    );
                }
            });
        }
        self.0.model.set(VoiceModel::Failed {
            generation,
            target,
            message,
        });
    }

    fn finish_remote(&self, generation: u64) {
        if !self.is_current(generation) {
            return;
        }
        let media = self.0.runtime.borrow_mut().media.take();
        if let Some(media) = media {
            media.close_now();
        }
        {
            let mut runtime = self.0.runtime.borrow_mut();
            runtime.target = None;
            runtime.session = None;
            runtime.generation = runtime.generation.wrapping_add(1);
            runtime.ice = IceBatcher::default();
            runtime.remote_ice = IceBatcher::default();
            runtime.answer_applied = false;
            runtime.remote_flush_active = false;
            runtime.signaling_state = SignalingState::NotStarted;
            runtime.connection_epoch = runtime.connection_epoch.wrapping_add(1);
            cancel_deadline(&mut runtime.negotiation_cancel);
            cancel_deadline(&mut runtime.disconnected_cancel);
        }
        self.0.model.set(VoiceModel::Idle);
    }
}

fn cancel_deadline(cancel: &mut Option<tokio::sync::oneshot::Sender<()>>) {
    if let Some(cancel) = cancel.take() {
        let _ = cancel.send(());
    }
}

fn callbacks(inner: Weak<Inner>, generation: u64) -> MediaCallbacks {
    let ice_inner = inner.clone();
    let failed_inner = inner.clone();
    let started_inner = inner.clone();
    let interrupted_inner = inner.clone();
    let recovered_inner = inner.clone();
    MediaCallbacks {
        ice_candidate: Rc::new(move |candidate| {
            if let Some(inner) = ice_inner.upgrade() {
                VoiceController::from_inner(inner).local_ice(generation, candidate);
            }
        }),
        connection_failed: Rc::new(move |message| {
            if let Some(inner) = failed_inner.upgrade() {
                VoiceController::from_inner(inner).fail_current(generation, message);
            }
        }),
        connection_interrupted: Rc::new(move || {
            if let Some(inner) = interrupted_inner.upgrade() {
                VoiceController::from_inner(inner).connection_interrupted(generation);
            }
        }),
        connection_recovered: Rc::new(move || {
            if let Some(inner) = recovered_inner.upgrade() {
                VoiceController::from_inner(inner).connection_recovered(generation);
            }
        }),
        remote_audio_started: Rc::new(move || {
            if let Some(inner) = started_inner.upgrade() {
                inner.model.update(|model| {
                    if let VoiceModel::Live {
                        generation: current,
                        playback_blocked,
                        ..
                    } = model
                        && *current == generation
                    {
                        *playback_blocked = false;
                    }
                });
            }
        }),
        playback_blocked: Rc::new(move || {
            if let Some(inner) = inner.upgrade() {
                inner.model.update(|model| {
                    if let VoiceModel::Live {
                        generation: current,
                        playback_blocked,
                        ..
                    } = model
                        && *current == generation
                    {
                        *playback_blocked = true;
                    }
                });
            }
        }),
    }
}

fn safe_media_error(error: String) -> String {
    let lower = error.to_ascii_lowercase();
    if lower.contains("permission") || lower.contains("notallowed") {
        "Microphone access is required for hands-free voice. Allow it in browser settings and try again.".to_owned()
    } else if lower.contains("route") || lower.contains("ice") || lower.contains("connection") {
        "Voice is unavailable: no direct route to this Tyde host. Connect both devices to the same LAN or VPN and try again.".to_owned()
    } else {
        "Voice could not start in this browser. Check microphone access and local-network connectivity.".to_owned()
    }
}

fn safe_signaling_error(_error: String) -> String {
    // Transport errors can contain broker endpoints or credential-bearing query
    // details. Voice UI and logs expose only this fixed, non-secret message.
    "Voice signaling failed. Check that this app and host are updated and connected.".to_owned()
}

fn bounded_voice_text(text: &str) -> String {
    const MAX_BYTES: usize = 4 * 1024;
    if text.len() <= MAX_BYTES {
        return text.to_owned();
    }
    let mut end = MAX_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

#[cfg(target_arch = "wasm32")]
fn spawn_voice(future: impl Future<Output = ()> + 'static) {
    wasm_bindgen_futures::spawn_local(future);
}

#[cfg(not(target_arch = "wasm32"))]
async fn sleep_voice(duration: Duration) {
    tokio::time::sleep(duration).await;
}

#[cfg(target_arch = "wasm32")]
async fn sleep_voice(duration: Duration) {
    wasmtimer::tokio::sleep(duration).await;
}

#[cfg(not(target_arch = "wasm32"))]
fn spawn_voice(future: impl Future<Output = ()> + 'static) {
    tokio::task::spawn_local(future);
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use crate::voice::media::{PreparedMedia, VoiceFuture};
    use crate::voice::model::{AudioProcessingReport, BrowserAudioSetting};

    #[derive(Default)]
    struct FakeMediaState {
        close_count: usize,
        muted: bool,
        interrupted: usize,
        resumed: usize,
        answers: usize,
        remote_candidates: usize,
        remote_complete: usize,
        emit_ice_during_prepare: bool,
        callbacks: Option<MediaCallbacks>,
    }

    struct FakeSession(Rc<RefCell<FakeMediaState>>);

    impl VoiceMediaSession for FakeSession {
        fn apply_answer(&self, _sdp: String) -> VoiceFuture<Result<(), String>> {
            self.0.borrow_mut().answers += 1;
            Box::pin(async { Ok(()) })
        }

        fn add_remote_candidate(
            &self,
            _candidate: IceCandidate,
        ) -> VoiceFuture<Result<(), String>> {
            self.0.borrow_mut().remote_candidates += 1;
            Box::pin(async { Ok(()) })
        }

        fn finish_remote_candidates(&self) -> VoiceFuture<Result<(), String>> {
            self.0.borrow_mut().remote_complete += 1;
            Box::pin(async { Ok(()) })
        }

        fn set_muted(&self, muted: bool) {
            self.0.borrow_mut().muted = muted;
        }

        fn interrupt_playback(&self) {
            self.0.borrow_mut().interrupted += 1;
        }

        fn resume_playback(&self) -> VoiceFuture<Result<(), String>> {
            self.0.borrow_mut().resumed += 1;
            Box::pin(async { Ok(()) })
        }

        fn close_now(&self) {
            self.0.borrow_mut().close_count += 1;
        }
    }

    struct FakeFactory {
        state: Rc<RefCell<FakeMediaState>>,
        events: Rc<RefCell<Vec<&'static str>>>,
    }

    impl VoiceMediaFactory for FakeFactory {
        fn prepare(&self, callbacks: MediaCallbacks) -> VoiceFuture<Result<PreparedMedia, String>> {
            let state = self.state.clone();
            let events = self.events.clone();
            Box::pin(async move {
                let emit_ice = state.borrow().emit_ice_during_prepare;
                state.borrow_mut().callbacks = Some(callbacks.clone());
                if emit_ice {
                    (callbacks.ice_candidate)(Some(candidate("candidate:local")));
                    (callbacks.ice_candidate)(None);
                }
                events.borrow_mut().push("media-prepared");
                Ok(PreparedMedia {
                    offer_sdp: "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111".to_owned(),
                    processing: AudioProcessingReport {
                        echo_cancellation: BrowserAudioSetting::Enabled,
                        ..Default::default()
                    },
                    session: Rc::new(FakeSession(state)),
                })
            })
        }
    }

    #[derive(Default)]
    struct FakeSignal {
        starts: RefCell<Vec<VoiceTarget>>,
        stops: RefCell<Vec<(VoiceTarget, &'static str)>>,
        events: Rc<RefCell<Vec<&'static str>>>,
    }

    impl VoiceSignaling for FakeSignal {
        fn start(
            &self,
            target: VoiceTarget,
            _session: VoiceSession,
            _offer_sdp: String,
        ) -> SignalFuture {
            self.starts.borrow_mut().push(target);
            self.events.borrow_mut().push("start");
            Box::pin(async { Ok(()) })
        }

        fn ice(
            &self,
            _target: VoiceTarget,
            _session: VoiceSession,
            _candidates: Vec<IceCandidate>,
            _complete: bool,
        ) -> SignalFuture {
            self.events.borrow_mut().push("ice");
            Box::pin(async { Ok(()) })
        }

        fn stop(
            &self,
            target: VoiceTarget,
            _session: VoiceSession,
            reason: &'static str,
        ) -> SignalFuture {
            self.stops.borrow_mut().push((target, reason));
            self.events.borrow_mut().push("stop");
            Box::pin(async { Ok(()) })
        }
    }

    fn target(name: &str) -> VoiceTarget {
        VoiceTarget {
            local_host_id: crate::state::LocalHostId("host".to_owned()),
            agent_id: protocol::AgentId(format!("agent-{name}")),
            instance_stream: protocol::StreamPath(format!("/agent/{name}")),
            agent_name: name.to_owned(),
        }
    }

    fn candidate(value: &str) -> IceCandidate {
        IceCandidate {
            candidate: value.to_owned(),
            sdp_mid: Some("audio".to_owned()),
            sdp_m_line_index: Some(0),
            username_fragment: None,
        }
    }

    fn setup() -> (VoiceController, Rc<RefCell<FakeMediaState>>, Rc<FakeSignal>) {
        let media = Rc::new(RefCell::new(FakeMediaState::default()));
        let signal = Rc::new(FakeSignal::default());
        let controller = VoiceController::with_dependencies(
            Rc::new(FakeFactory {
                state: media.clone(),
                events: signal.events.clone(),
            }),
            signal.clone(),
        );
        (controller, media, signal)
    }

    fn current(controller: &VoiceController) -> (u64, VoiceSession) {
        let runtime = controller.0.runtime.borrow();
        (runtime.generation, runtime.session.clone().unwrap())
    }

    #[tokio::test]
    async fn focus_change_and_user_done_close_media_with_only_voice_stops() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (controller, media, signal) = setup();
                controller.start(target("One"));
                tokio::task::yield_now().await;
                controller.terminate_if_target_changed(Some(target("Two")));
                tokio::task::yield_now().await;
                assert_eq!(media.borrow().close_count, 1);
                assert_eq!(signal.stops.borrow().len(), 1);
                assert_eq!(signal.stops.borrow()[0].1, "focus-changed");
                assert_eq!(controller.model().get_untracked(), VoiceModel::Idle);

                controller.start(target("Two"));
                tokio::task::yield_now().await;
                assert_eq!(signal.starts.borrow().len(), 2);
                controller.end("user-done");
                tokio::task::yield_now().await;
                assert_eq!(media.borrow().close_count, 2);
                assert_eq!(signal.stops.borrow().len(), 2);
                assert_eq!(signal.stops.borrow()[1].1, "user-done");
                assert_eq!(controller.model().get_untracked(), VoiceModel::Idle);
            })
            .await;
    }

    #[tokio::test]
    async fn stale_generation_cannot_attach_an_answer() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (controller, media, _) = setup();
                controller.start(target("One"));
                tokio::task::yield_now().await;
                let (generation, session) = current(&controller);
                controller.end("done");
                controller.opened(generation, &target("One"), session, "answer".to_owned());
                tokio::task::yield_now().await;
                assert_eq!(media.borrow().answers, 0);
                assert_eq!(controller.model().get_untracked(), VoiceModel::Idle);
            })
            .await;
    }

    #[tokio::test]
    async fn interrupt_exposes_recovery_and_next_response_restores_playback() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (controller, media, _) = setup();
                controller.start(target("One"));
                tokio::task::yield_now().await;
                controller.interrupt();
                assert_eq!(media.borrow().interrupted, 1);
                assert!(matches!(
                    controller.model().get_untracked(),
                    VoiceModel::Live {
                        playback_blocked: true,
                        ..
                    }
                ));
                let (_, session) = current(&controller);
                for state in [
                    protocol::VoiceSessionState::Listening,
                    protocol::VoiceSessionState::Speaking,
                ] {
                    controller.apply_state(
                        &target("One").local_host_id,
                        &session.stream,
                        VoiceStatePayload {
                            session_id: VoiceSessionId(session.id.clone()),
                            state,
                            progress: None,
                            caption: None,
                            transcript: None,
                            ended_reason: None,
                        },
                    );
                }
                tokio::task::yield_now().await;
                assert_eq!(media.borrow().resumed, 1);
                assert!(matches!(
                    controller.model().get_untracked(),
                    VoiceModel::Live {
                        playback_blocked: false,
                        ..
                    }
                ));
            })
            .await;
    }

    #[tokio::test]
    async fn negotiation_timeout_contains_the_live_microphone() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (controller, media, signal) = setup();
                controller.start(target("One"));
                tokio::task::yield_now().await;
                let (generation, _) = current(&controller);
                controller.expire_negotiation(generation);
                tokio::task::yield_now().await;
                assert_eq!(media.borrow().close_count, 1);
                assert!(matches!(
                    controller.model().get_untracked(),
                    VoiceModel::Failed { .. }
                ));
                assert_eq!(signal.stops.borrow().len(), 1);
            })
            .await;
    }

    #[tokio::test]
    async fn temporary_disconnection_recovers_within_the_grace_period() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (controller, media, _) = setup();
                controller.start(target("One"));
                tokio::task::yield_now().await;
                let generation = current(&controller).0;
                let callbacks = media.borrow().callbacks.clone().unwrap();
                (callbacks.connection_interrupted)();
                let interrupted_epoch = controller.0.runtime.borrow().connection_epoch;
                (callbacks.connection_recovered)();
                controller.expire_disconnected(generation, interrupted_epoch);
                assert_eq!(media.borrow().close_count, 0);

                (callbacks.connection_interrupted)();
                let expired_epoch = controller.0.runtime.borrow().connection_epoch;
                controller.expire_disconnected(generation, expired_epoch);
                assert_eq!(media.borrow().close_count, 1);
            })
            .await;
    }

    #[tokio::test]
    async fn inbound_frames_require_exact_host_stream_and_session_correlation() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (controller, media, _) = setup();
                controller.start(target("One"));
                tokio::task::yield_now().await;
                let (generation, session) = current(&controller);
                let wrong_host = crate::state::LocalHostId("wrong".to_owned());
                let wrong_stream = protocol::StreamPath("/voice/wrong".to_owned());
                let wrong_session = VoiceSessionId("wrong".to_owned());

                controller.apply_answer(
                    &wrong_host,
                    &session.stream,
                    VoiceAnswerPayload {
                        session_id: VoiceSessionId(session.id.clone()),
                        sdp: "answer".to_owned(),
                    },
                );
                controller.apply_answer(
                    &target("One").local_host_id,
                    &wrong_stream,
                    VoiceAnswerPayload {
                        session_id: VoiceSessionId(session.id.clone()),
                        sdp: "answer".to_owned(),
                    },
                );
                controller.apply_state(
                    &target("One").local_host_id,
                    &session.stream,
                    VoiceStatePayload {
                        session_id: wrong_session,
                        state: protocol::VoiceSessionState::Listening,
                        progress: None,
                        caption: Some("stale text".to_owned()),
                        transcript: Some(protocol::VoiceTranscript {
                            speaker: protocol::VoiceTranscriptSpeaker::User,
                            text: "stale text".to_owned(),
                            is_final: true,
                        }),
                        ended_reason: None,
                    },
                );
                tokio::task::yield_now().await;
                assert_eq!(media.borrow().answers, 0);
                assert!(matches!(
                    controller.model().get_untracked(),
                    VoiceModel::Live {
                        generation: current,
                        phase: VoicePhase::Connecting,
                        ..
                    } if current == generation
                ));

                controller.apply_answer(
                    &target("One").local_host_id,
                    &session.stream,
                    VoiceAnswerPayload {
                        session_id: VoiceSessionId(session.id.clone()),
                        sdp: "answer".to_owned(),
                    },
                );
                controller.apply_remote_ice(
                    &target("One").local_host_id,
                    &session.stream,
                    VoiceIceCandidatePayload {
                        session_id: VoiceSessionId(session.id.clone()),
                        candidates: vec![protocol::VoiceIceCandidate {
                            candidate: "candidate:remote".to_owned(),
                            sdp_mid: Some("audio".to_owned()),
                            sdp_m_line_index: Some(0),
                        }],
                    },
                );
                controller.apply_remote_ice_complete(
                    &target("One").local_host_id,
                    &session.stream,
                    VoiceIceCandidatesCompletePayload {
                        session_id: VoiceSessionId(session.id.clone()),
                    },
                );
                controller.apply_state(
                    &target("One").local_host_id,
                    &session.stream,
                    VoiceStatePayload {
                        session_id: VoiceSessionId(session.id),
                        state: protocol::VoiceSessionState::Listening,
                        progress: None,
                        caption: Some("heard exactly".to_owned()),
                        transcript: Some(protocol::VoiceTranscript {
                            speaker: protocol::VoiceTranscriptSpeaker::User,
                            text: "heard exactly".to_owned(),
                            is_final: true,
                        }),
                        ended_reason: None,
                    },
                );
                tokio::task::yield_now().await;
                tokio::task::yield_now().await;
                tokio::task::yield_now().await;
                assert_eq!(media.borrow().answers, 1);
                assert_eq!(media.borrow().remote_candidates, 1);
                assert_eq!(media.borrow().remote_complete, 1);
                assert!(matches!(
                    controller.model().get_untracked(),
                    VoiceModel::Live {
                        phase: VoicePhase::Listening,
                        caption: Some(caption),
                        transcript: Some(transcript),
                        ..
                    } if caption == "heard exactly"
                        && transcript.text == "heard exactly"
                        && transcript.is_final
                ));
            })
            .await;
    }

    #[tokio::test]
    async fn mobile_prepares_media_then_starts_before_queued_ice() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (controller, media, signal) = setup();
                media.borrow_mut().emit_ice_during_prepare = true;
                controller.start(target("One"));
                tokio::task::yield_now().await;
                tokio::task::yield_now().await;
                assert_eq!(
                    signal.events.borrow().as_slice(),
                    ["media-prepared", "start", "ice"]
                );
            })
            .await;
    }

    #[test]
    fn availability_is_typed_and_actionable() {
        let state = AppState::new();
        let host = crate::state::LocalHostId("host".to_owned());
        state.active_agent.set(Some(crate::state::ActiveAgentRef {
            local_host_id: host.clone(),
            agent_id: protocol::AgentId("agent".to_owned()),
        }));
        state.connection_statuses.update(|statuses| {
            statuses.insert(host.clone(), ConnectionStatus::Connected);
        });
        let mut settings = protocol::HostSettings::default();
        settings.voice.availability = protocol::VoiceAvailability::Unavailable {
            reason: protocol::VoiceUnavailableReason::NoReachableCandidate,
        };
        state.host_settings_by_host.update(|all| {
            all.insert(host.clone(), settings.clone());
        });
        assert_eq!(
            VoiceController::entry_availability_for_active_agent(&state),
            VoiceEntryAvailability::Unavailable("Same LAN or VPN required for voice.")
        );
        settings.voice.availability = protocol::VoiceAvailability::Available {
            direct_connections_only: true,
        };
        state.host_settings_by_host.update(|all| {
            all.insert(host, settings);
        });
        assert_eq!(
            VoiceController::entry_availability_for_active_agent(&state),
            VoiceEntryAvailability::Available
        );
    }
}
