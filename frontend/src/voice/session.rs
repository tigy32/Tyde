//! Pure desktop voice-session state machine.
//!
//! This module has no browser, DOM, or protocol-transport dependency. It owns
//! every rule that decides *whether* something may happen — target binding,
//! generation guarding, teardown idempotence — so those rules can be tested
//! without mounting a component or standing up a media stack.
//!
//! Two invariants drive the whole design:
//!
//! 1. **The target is immutable for the life of a session.** A session is bound
//!    to one `(host_id, agent_id, instance_stream)` triple at start and never
//!    retargets. `instance_stream` is in the key on purpose: it is the agent's
//!    connection generation, so a reconnect that replaces the stream is a
//!    different target even though the agent id is unchanged.
//!
//! 2. **Every asynchronous completion is generation-guarded.** Permission
//!    grants, SDP answers, ICE candidates, and server state updates all arrive
//!    after the fact and can outlive the session that asked for them. Each
//!    carries the `VoiceGeneration` it was issued under; anything stale is
//!    dropped rather than applied to whatever session happens to be current.

use protocol::StreamPath;

use crate::state::ActiveAgentRef;

/// The immutable agent binding for one voice session.
///
/// `instance_stream` is part of the identity, not decoration: `AgentInfo`
/// carries a fresh `instance_stream` per connection generation, so comparing it
/// is what distinguishes "same agent, still connected" from "same agent id,
/// reconnected underneath us".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VoiceTarget {
    pub agent: ActiveAgentRef,
    pub instance_stream: StreamPath,
}

impl VoiceTarget {
    pub fn new(agent: ActiveAgentRef, instance_stream: StreamPath) -> Self {
        Self {
            agent,
            instance_stream,
        }
    }

    pub fn host_id(&self) -> &str {
        &self.agent.host_id
    }
}

/// Monotonic session counter, bumped by [`VoiceUiState::begin`]. A value is
/// never reused, so work issued for one session — a permission grant, an SDP
/// answer, a media callback — can always be told apart from work belonging to
/// the next one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct VoiceGeneration(pub u64);

impl VoiceGeneration {
    fn next(self) -> Self {
        Self(self.0.wrapping_add(1))
    }
}

/// The session identity and its `/voice/<id>` stream. The **client** mints
/// both at start (`protocol::VoiceStartPayload.session_id`) and the host echoes
/// the id back in `VoiceReady`, so the stream path is derivable from the id
/// alone and the two can never disagree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VoiceSessionKey {
    pub session_id: String,
    pub stream: StreamPath,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VoicePhase {
    Idle,
    /// `getUserMedia` is outstanding. The OS permission sheet may be up.
    RequestingMic,
    /// Offer sent; waiting for the server's answer and ICE to converge.
    Negotiating,
    Live,
    /// Local teardown has already run; the stop frame is best-effort in flight.
    Ending,
    Failed,
}

impl VoicePhase {
    /// Phases in which a session owns media and a server session may exist.
    pub fn is_engaged(self) -> bool {
        matches!(
            self,
            VoicePhase::RequestingMic | VoicePhase::Negotiating | VoicePhase::Live
        )
    }
}

/// Why a session ended. Every variant is rendered to the user verbatim, so
/// there is no "unknown"/"other" catch-all to hide behind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VoiceEndReason {
    UserRequested,
    FocusedAgentChanged,
    AgentGone,
    AgentFatal,
    InstanceStreamChanged,
    HostDisconnected,
    WindowHidden,
    PageTeardown,
    ServerEnded,
    TransportFailed,
    MediaFailed,
    PermissionDenied,
}

impl VoiceEndReason {
    pub fn label(self) -> &'static str {
        match self {
            VoiceEndReason::UserRequested => "Voice ended",
            VoiceEndReason::FocusedAgentChanged => "Voice ended — you switched to another agent",
            VoiceEndReason::AgentGone => "Voice ended — the agent was closed",
            VoiceEndReason::AgentFatal => "Voice ended — the agent stopped with an error",
            VoiceEndReason::InstanceStreamChanged => "Voice ended — the agent reconnected",
            VoiceEndReason::HostDisconnected => "Voice ended — the host disconnected",
            VoiceEndReason::WindowHidden => "Voice ended — the window was hidden",
            VoiceEndReason::PageTeardown => "Voice ended",
            VoiceEndReason::ServerEnded => "Voice ended by the host",
            VoiceEndReason::TransportFailed => "Voice ended — the connection dropped",
            VoiceEndReason::MediaFailed => "Voice ended — audio stopped working",
            VoiceEndReason::PermissionDenied => "Voice ended — microphone access was refused",
        }
    }

    /// True when the end was not something the user asked for, so the strip
    /// should linger with an explanation instead of disappearing silently.
    pub fn is_involuntary(self) -> bool {
        !matches!(
            self,
            VoiceEndReason::UserRequested | VoiceEndReason::PageTeardown
        )
    }
}

/// What the conversation is doing right now.
///
/// Server-owned: every variant is a projection of a `VoiceSessionState` the
/// host sent. There is deliberately no "you are speaking" variant, because the
/// landed protocol (`protocol::VoiceSessionState`) reports no client-side
/// voice activity — inventing one from local audio would be a claim the server
/// never made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VoiceActivity {
    Connecting,
    Listening,
    AgentSpeaking,
    AgentWorking,
    Ending,
}

impl VoiceActivity {
    pub fn label(self) -> &'static str {
        match self {
            VoiceActivity::Connecting => "Connecting",
            VoiceActivity::Listening => "Listening",
            VoiceActivity::AgentSpeaking => "Speaking",
            VoiceActivity::AgentWorking => "Working",
            VoiceActivity::Ending => "Ending",
        }
    }
}

/// Observed echo-cancellation state. Deliberately distinguishes *asked for*
/// from *actually applied*: `getUserMedia` accepting an `echoCancellation`
/// constraint is not evidence the platform is cancelling anything, and the UI
/// must never imply otherwise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AecStatus {
    /// No track acquired yet.
    Unknown,
    /// Requested, but the track reported no `echoCancellation` setting at all,
    /// so we genuinely do not know.
    RequestedUnreported,
    /// The track reports `echoCancellation: true`.
    Confirmed,
    /// The track reports `echoCancellation: false` — we asked and did not get it.
    NotApplied,
}

impl AecStatus {
    pub fn label(self) -> &'static str {
        match self {
            AecStatus::Unknown => "AEC unknown",
            AecStatus::RequestedUnreported => "AEC requested, not reported",
            AecStatus::Confirmed => "AEC on",
            AecStatus::NotApplied => "AEC off",
        }
    }

    /// Only `Confirmed` may be presented as a positive state.
    pub fn is_warning(self) -> bool {
        !matches!(self, AecStatus::Confirmed)
    }
}

/// Audio-processing settings the acquired track actually reports, read back
/// from `MediaStreamTrack.getSettings()`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EffectiveAudio {
    pub echo_cancellation: Option<bool>,
    pub noise_suppression: Option<bool>,
    pub auto_gain_control: Option<bool>,
    pub sample_rate: Option<u32>,
}

impl EffectiveAudio {
    pub fn aec_status(&self) -> AecStatus {
        match self.echo_cancellation {
            Some(true) => AecStatus::Confirmed,
            Some(false) => AecStatus::NotApplied,
            None => AecStatus::RequestedUnreported,
        }
    }

    /// One-line diagnostics summary. Every field says what was observed, never
    /// what was requested.
    pub fn summary(&self) -> String {
        fn flag(name: &str, value: Option<bool>) -> String {
            match value {
                Some(true) => format!("{name}: on"),
                Some(false) => format!("{name}: off"),
                None => format!("{name}: not reported"),
            }
        }
        let mut parts = vec![
            flag("Echo cancellation", self.echo_cancellation),
            flag("Noise suppression", self.noise_suppression),
            flag("Auto gain", self.auto_gain_control),
        ];
        if let Some(rate) = self.sample_rate {
            parts.push(format!("Sample rate: {rate} Hz"));
        }
        parts.join(" · ")
    }
}

/// A failure the user must see. `message` is always the real underlying text —
/// a rejected `getUserMedia`, a server error string — never a generic
/// substitute.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VoiceFailure {
    pub stage: VoiceStage,
    pub message: String,
    pub retryable: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VoiceStage {
    Microphone,
    Negotiation,
    Signaling,
    Media,
    Server,
}

impl VoiceStage {
    pub fn label(self) -> &'static str {
        match self {
            VoiceStage::Microphone => "Microphone",
            VoiceStage::Negotiation => "Setup",
            VoiceStage::Signaling => "Signaling",
            VoiceStage::Media => "Audio",
            VoiceStage::Server => "Host",
        }
    }
}

/// Why the mic control is not offered for a given chat. Each variant carries
/// its own visible sentence — a disabled control that does not say why is a
/// silent failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VoiceUnavailable {
    NoAgent,
    AgentNotStarted,
    AgentTerminated,
    HostDisconnected,
    /// Server-declared, carrying the server's own reason text.
    HostReported(String),
    /// A session is live on a different chat.
    BusyElsewhere,
}

impl VoiceUnavailable {
    pub fn reason(&self) -> String {
        match self {
            VoiceUnavailable::NoAgent => "Start or open a chat to use voice".to_owned(),
            VoiceUnavailable::AgentNotStarted => {
                "Voice is available once the agent starts".to_owned()
            }
            VoiceUnavailable::AgentTerminated => "This agent has stopped".to_owned(),
            VoiceUnavailable::HostDisconnected => "This host is not connected".to_owned(),
            VoiceUnavailable::HostReported(reason) => reason.clone(),
            VoiceUnavailable::BusyElsewhere => "End the current voice session first".to_owned(),
        }
    }
}

/// Bytes of a single transcript line the strip will show. The host already
/// clamps to 4 KiB (`server/src/voice.rs`), but a client that renders whatever
/// arrives is one server bug away from an unbounded row, so the display is
/// bounded independently.
pub const MAX_TRANSCRIPT_LINE_BYTES: usize = 512;

/// Finalised transcript lines kept for display. Voice is a transient surface;
/// the durable record is the ordinary chat transcript underneath.
pub const MAX_TRANSCRIPT_LINES: usize = 3;

/// Who said a transcript line. Server-declared
/// (`protocol::VoiceTranscriptSpeaker`); never inferred from timing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TranscriptSpeaker {
    User,
    Agent,
}

impl TranscriptSpeaker {
    pub fn label(self) -> &'static str {
        match self {
            TranscriptSpeaker::User => "You",
            TranscriptSpeaker::Agent => "Agent",
        }
    }
}

/// One line of what was actually said, as reported by the host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TranscriptLine {
    pub speaker: TranscriptSpeaker,
    pub text: String,
    pub is_final: bool,
}

impl TranscriptLine {
    /// Trim to a displayable length on a character boundary, marking the cut so
    /// a truncated line never reads as a complete utterance.
    pub fn bounded(speaker: TranscriptSpeaker, text: String, is_final: bool) -> Self {
        let text = if text.len() > MAX_TRANSCRIPT_LINE_BYTES {
            let mut cut = MAX_TRANSCRIPT_LINE_BYTES;
            while cut > 0 && !text.is_char_boundary(cut) {
                cut -= 1;
            }
            format!("{}…", &text[..cut])
        } else {
            text
        };
        Self {
            speaker,
            text,
            is_final,
        }
    }
}

/// A single line of passive progress.
///
/// The **server** decides that a real agent event happened and which kind it
/// was (`protocol::VoiceAgentProgress { source_seq, source_kind }`); the client
/// only phrases it. `sequence` is the server's `source_seq` — the identity of
/// the originating agent event — which is what lets replays and out-of-order
/// deliveries be dropped instead of shown twice.
///
/// There is no path in this type that produces a line without a server-sent
/// `source_seq`: progress cannot be manufactured locally.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VoiceProgressLine {
    pub sequence: u64,
    pub text: String,
}

/// The number of progress lines kept for display. Voice is a transient
/// surface; an unbounded list would grow for the life of the session.
pub const MAX_PROGRESS_LINES: usize = 4;

/// Everything the UI renders. Plain data only — `Send + Sync`, no browser
/// handles — so it can live in an ordinary reactive signal. Media handles live
/// in [`crate::voice::media`], reached through a thread-local.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VoiceUiState {
    pub phase: VoicePhase,
    pub generation: VoiceGeneration,
    pub target: Option<VoiceTarget>,
    pub session: Option<VoiceSessionKey>,
    pub activity: VoiceActivity,
    pub muted: bool,
    pub effective_audio: EffectiveAudio,
    pub aec: AecStatus,
    /// True when the host reported it can only reach this client over direct
    /// candidates (no relay). Surfaced in diagnostics because it is the
    /// difference between "voice will work off-LAN" and "voice will not".
    pub direct_connections_only: bool,
    /// True once the host has answered `VoiceReady` for this session. Until
    /// then nothing has confirmed the host even created a session, which is
    /// what the negotiation deadline exists to bound.
    pub host_admitted: bool,
    /// What is being said right now, from the host's `caption`. Live and
    /// possibly partial.
    ///
    /// Only replaced when a frame actually carries one: a `VoiceState` with no
    /// caption means "no new caption", not "nothing is being said". Clearing on
    /// every captionless state change would blank the line on the next
    /// progress update.
    pub caption: Option<String>,
    /// Finalised lines, oldest first, bounded by [`MAX_TRANSCRIPT_LINES`].
    pub transcript: Vec<TranscriptLine>,
    /// The most recent tool the agent started on the voice session's behalf,
    /// derived from a server-sent `ToolStarted`/`ToolProgressed` progress
    /// event. Shown so the user can see what voice mode is doing for them.
    pub tool_notice: Option<String>,
    pub progress: Vec<VoiceProgressLine>,
    pub last_progress_sequence: u64,
    /// A problem the session survived — a rejected Nova tool call, a refused
    /// mute, a transient ICE `disconnected`. Shown in the strip while the
    /// session stays engaged and the End control stays available.
    ///
    /// Deliberately separate from `failure`: a non-fatal problem must never
    /// move the phase out of engaged, because the phase is what the UI uses to
    /// decide whether to offer End, and a session whose media is still live
    /// must always be endable.
    pub warning: Option<VoiceFailure>,
    /// The failure that *ended* the session. Only ever set after teardown.
    pub failure: Option<VoiceFailure>,
    pub ended_reason: Option<VoiceEndReason>,
    /// Set when the remote audio element refused to play. Voice is still
    /// connected; the user needs one gesture to unblock output.
    pub playback_blocked: bool,
}

impl Default for VoiceUiState {
    fn default() -> Self {
        Self {
            phase: VoicePhase::Idle,
            generation: VoiceGeneration::default(),
            target: None,
            session: None,
            activity: VoiceActivity::Connecting,
            muted: false,
            effective_audio: EffectiveAudio::default(),
            aec: AecStatus::Unknown,
            direct_connections_only: false,
            host_admitted: false,
            caption: None,
            transcript: Vec::new(),
            tool_notice: None,
            progress: Vec::new(),
            last_progress_sequence: 0,
            warning: None,
            failure: None,
            ended_reason: None,
            playback_blocked: false,
        }
    }
}

/// Returned by [`VoiceUiState::end`] on the *first* call for a session. `None`
/// on every later call, which is what makes teardown idempotent: the caller
/// stops media and sends the stop frame only when it gets a `Some`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EndedSession {
    pub generation: VoiceGeneration,
    pub target: VoiceTarget,
    pub session: Option<VoiceSessionKey>,
    pub reason: VoiceEndReason,
}

impl VoiceUiState {
    /// Is this state bound to `target`? Used by the focus guard.
    pub fn is_bound_to(&self, target: &VoiceTarget) -> bool {
        self.target.as_ref() == Some(target)
    }

    pub fn bound_agent(&self) -> Option<&ActiveAgentRef> {
        self.target.as_ref().map(|target| &target.agent)
    }

    /// True when a session owns media or a server session right now.
    pub fn is_engaged(&self) -> bool {
        self.phase.is_engaged()
    }

    /// Begin a new session. Any previous session must already have been ended
    /// by the caller; this clears residual display state either way so a stale
    /// progress line or error cannot bleed into a fresh session.
    ///
    /// The session key is minted by the client before the first frame, because
    /// `protocol::VoiceStartPayload` carries the `session_id` and the
    /// `/voice/<id>` stream is derived from it.
    pub fn begin(&mut self, target: VoiceTarget, session: VoiceSessionKey) -> VoiceGeneration {
        let generation = self.generation.next();
        *self = Self {
            phase: VoicePhase::RequestingMic,
            generation,
            target: Some(target),
            session: Some(session),
            activity: VoiceActivity::Connecting,
            ..Self::default()
        };
        generation
    }

    /// Microphone acquired. Records the settings the track actually reports.
    pub fn mic_granted(&mut self, generation: VoiceGeneration, audio: EffectiveAudio) -> bool {
        if !self.accepts(generation, VoicePhase::RequestingMic) {
            return false;
        }
        self.effective_audio = audio;
        self.aec = audio.aec_status();
        self.phase = VoicePhase::Negotiating;
        true
    }

    /// The host accepted the session (`VoiceReady`).
    ///
    /// Accepted in `RequestingMic` as well as `Negotiating`. The client sends
    /// `VoiceStart` before prompting for the microphone, so that the host can
    /// refuse (voice disabled, agent gone, another session live) before the
    /// user is asked; the host answers as soon as its provider is up, which can
    /// easily be while the permission sheet is still open. Requiring
    /// `Negotiating` here would silently drop a valid Ready.
    pub fn ready(&mut self, generation: VoiceGeneration, direct_connections_only: bool) -> bool {
        if generation != self.generation
            || !matches!(
                self.phase,
                VoicePhase::RequestingMic | VoicePhase::Negotiating
            )
        {
            return false;
        }
        self.direct_connections_only = direct_connections_only;
        self.host_admitted = true;
        true
    }

    /// Record a problem the session survived. Never changes the phase, so the
    /// End control stays available and media ownership is untouched.
    pub fn warn(&mut self, generation: VoiceGeneration, warning: VoiceFailure) -> bool {
        if generation != self.generation || !self.phase.is_engaged() {
            return false;
        }
        if self.warning.as_ref() == Some(&warning) {
            return false;
        }
        self.warning = Some(warning);
        true
    }

    /// Clear a warning once the condition that caused it is gone.
    pub fn clear_warning(&mut self, generation: VoiceGeneration) -> bool {
        if generation != self.generation || self.warning.is_none() {
            return false;
        }
        self.warning = None;
        true
    }

    /// Media is flowing.
    pub fn connected(&mut self, generation: VoiceGeneration) -> bool {
        if !self.accepts(generation, VoicePhase::Negotiating) {
            return false;
        }
        self.phase = VoicePhase::Live;
        self.activity = VoiceActivity::Listening;
        true
    }

    pub fn set_activity(&mut self, generation: VoiceGeneration, activity: VoiceActivity) -> bool {
        if !self.accepts_live(generation) {
            return false;
        }
        if self.activity == activity {
            return false;
        }
        self.activity = activity;
        true
    }

    pub fn set_muted(&mut self, generation: VoiceGeneration, muted: bool) -> bool {
        if !self.accepts_engaged(generation) || self.muted == muted {
            return false;
        }
        self.muted = muted;
        true
    }

    /// Record the live caption. Absent input is ignored rather than clearing —
    /// see the field comment.
    pub fn set_caption(&mut self, generation: VoiceGeneration, caption: Option<String>) -> bool {
        let Some(caption) = caption else {
            return false;
        };
        if !self.accepts_live(generation) || self.caption.as_deref() == Some(caption.as_str()) {
            return false;
        }
        self.caption = Some(caption);
        true
    }

    /// Append or replace a transcript line.
    ///
    /// A non-final line supersedes a previous non-final line from the same
    /// speaker — that is a partial being revised, not a second utterance.
    pub fn push_transcript(&mut self, generation: VoiceGeneration, line: TranscriptLine) -> bool {
        if !self.accepts_live(generation) || line.text.is_empty() {
            return false;
        }
        if let Some(last) = self.transcript.last_mut()
            && !last.is_final
            && last.speaker == line.speaker
        {
            *last = line;
            return true;
        }
        self.transcript.push(line);
        let overflow = self.transcript.len().saturating_sub(MAX_TRANSCRIPT_LINES);
        if overflow > 0 {
            self.transcript.drain(0..overflow);
        }
        true
    }

    pub fn set_tool_notice(&mut self, generation: VoiceGeneration, notice: Option<String>) -> bool {
        if !self.accepts_live(generation) || self.tool_notice == notice {
            return false;
        }
        self.tool_notice = notice;
        true
    }

    pub fn set_playback_blocked(&mut self, generation: VoiceGeneration, blocked: bool) -> bool {
        if !self.accepts_engaged(generation) || self.playback_blocked == blocked {
            return false;
        }
        self.playback_blocked = blocked;
        true
    }

    /// Append a server-projected progress line. Duplicate and out-of-order
    /// sequences are dropped, so a replay cannot make the agent appear to
    /// repeat work it did once.
    pub fn push_progress(&mut self, generation: VoiceGeneration, line: VoiceProgressLine) -> bool {
        if !self.accepts_live(generation) {
            return false;
        }
        if line.sequence <= self.last_progress_sequence {
            return false;
        }
        self.last_progress_sequence = line.sequence;
        self.progress.push(line);
        let overflow = self.progress.len().saturating_sub(MAX_PROGRESS_LINES);
        if overflow > 0 {
            self.progress.drain(0..overflow);
        }
        true
    }

    /// Record a failure. This does not tear down on its own — the caller ends
    /// the session and then calls this so the strip can explain what happened.
    ///
    /// Accepts the *ending* session's generation, which is why `end` leaves the
    /// generation alone. Rejected once the strip is idle, so a late failure
    /// cannot resurrect a banner the user already dismissed.
    pub fn fail(&mut self, generation: VoiceGeneration, failure: VoiceFailure) -> bool {
        if generation != self.generation || matches!(self.phase, VoicePhase::Idle) {
            return false;
        }
        self.phase = VoicePhase::Failed;
        self.failure = Some(failure);
        self.warning = None;
        self.caption = None;
        self.tool_notice = None;
        true
    }

    /// End the session. Idempotent: returns `Some` exactly once per session,
    /// and the caller performs media teardown and the stop frame only then.
    ///
    /// The generation deliberately does **not** move here. It identifies the
    /// session, and the session that just ended still needs to be addressable
    /// — `fail` records the error that caused the teardown against it. What
    /// makes late callbacks harmless is the phase: once this returns, the
    /// phase is `Ending`, and every `accepts*` gate below requires an engaged
    /// or `Live` phase. `begin` is what moves the generation, so work issued
    /// for one session can never touch the next.
    pub fn end(&mut self, reason: VoiceEndReason) -> Option<EndedSession> {
        let target = self.target.clone()?;
        if !self.phase.is_engaged() {
            return None;
        }
        let ended = EndedSession {
            generation: self.generation,
            target,
            session: self.session.clone(),
            reason,
        };
        self.phase = VoicePhase::Ending;
        self.ended_reason = Some(reason);
        self.activity = VoiceActivity::Ending;
        self.warning = None;
        self.caption = None;
        self.tool_notice = None;
        self.playback_blocked = false;
        Some(ended)
    }

    /// Record a refusal that happened *before* a session existed — a missing
    /// audio backend, for instance. Distinct from [`Self::fail`] because there
    /// is no generation to check and no session to end; the point is only that
    /// the user sees why the control did nothing.
    pub fn fail_to_start(&mut self, failure: VoiceFailure) -> bool {
        if self.phase.is_engaged() {
            return false;
        }
        self.phase = VoicePhase::Failed;
        self.failure = Some(failure);
        true
    }

    /// Drop the residual "why it ended"/"what failed" banner, returning the
    /// strip to fully idle. Called when the user dismisses it or starts again.
    pub fn dismiss(&mut self) -> bool {
        if self.phase.is_engaged() {
            return false;
        }
        if matches!(self.phase, VoicePhase::Idle) && self.ended_reason.is_none() {
            return false;
        }
        let generation = self.generation;
        *self = Self {
            generation,
            ..Self::default()
        };
        true
    }

    fn accepts(&self, generation: VoiceGeneration, expected: VoicePhase) -> bool {
        generation == self.generation && self.phase == expected
    }

    fn accepts_live(&self, generation: VoiceGeneration) -> bool {
        generation == self.generation && matches!(self.phase, VoicePhase::Live)
    }

    fn accepts_engaged(&self, generation: VoiceGeneration) -> bool {
        generation == self.generation && self.phase.is_engaged()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(agent: &str, stream: &str) -> VoiceTarget {
        VoiceTarget::new(
            ActiveAgentRef {
                host_id: "host-1".to_owned(),
                agent_id: protocol::AgentId(agent.to_owned()),
            },
            StreamPath(stream.to_owned()),
        )
    }

    fn key(id: &str) -> VoiceSessionKey {
        VoiceSessionKey {
            session_id: id.to_owned(),
            stream: StreamPath(format!("/voice/{id}")),
        }
    }

    fn live() -> (VoiceUiState, VoiceGeneration) {
        let mut state = VoiceUiState::default();
        let generation = state.begin(target("a", "/agent/a"), key("s1"));
        assert!(state.mic_granted(
            generation,
            EffectiveAudio {
                echo_cancellation: Some(true),
                ..EffectiveAudio::default()
            }
        ));
        assert!(state.ready(generation, true));
        assert!(state.connected(generation));
        (state, generation)
    }

    #[test]
    fn happy_path_reaches_live_and_reports_confirmed_aec() {
        let (state, _) = live();
        assert_eq!(state.phase, VoicePhase::Live);
        assert_eq!(state.activity, VoiceActivity::Listening);
        assert_eq!(state.aec, AecStatus::Confirmed);
        assert_eq!(
            state.session.as_ref().map(|s| s.session_id.as_str()),
            Some("s1")
        );
        assert_eq!(
            state.session.as_ref().map(|s| s.stream.0.as_str()),
            Some("/voice/s1"),
            "the voice stream is derived from the client-minted session id"
        );
        assert!(
            state.direct_connections_only,
            "the host's reachability constraint is recorded for diagnostics"
        );
    }

    #[test]
    fn aec_distinguishes_requested_from_applied_from_unreported() {
        assert_eq!(
            EffectiveAudio {
                echo_cancellation: Some(true),
                ..EffectiveAudio::default()
            }
            .aec_status(),
            AecStatus::Confirmed
        );
        assert_eq!(
            EffectiveAudio {
                echo_cancellation: Some(false),
                ..EffectiveAudio::default()
            }
            .aec_status(),
            AecStatus::NotApplied
        );
        assert_eq!(
            EffectiveAudio::default().aec_status(),
            AecStatus::RequestedUnreported
        );
        // Only a positively observed `true` may read as a good state.
        assert!(!AecStatus::Confirmed.is_warning());
        assert!(AecStatus::RequestedUnreported.is_warning());
        assert!(AecStatus::NotApplied.is_warning());
    }

    #[test]
    fn effective_audio_summary_never_claims_unreported_settings() {
        let summary = EffectiveAudio {
            echo_cancellation: None,
            noise_suppression: Some(false),
            auto_gain_control: Some(true),
            sample_rate: Some(48_000),
        }
        .summary();
        assert!(summary.contains("Echo cancellation: not reported"));
        assert!(summary.contains("Noise suppression: off"));
        assert!(summary.contains("Auto gain: on"));
        assert!(summary.contains("48000 Hz"));
    }

    #[test]
    fn end_is_idempotent_and_only_the_first_call_asks_for_teardown() {
        let (mut state, _) = live();
        let first = state.end(VoiceEndReason::UserRequested);
        assert!(first.is_some(), "first end must request teardown");
        assert_eq!(first.unwrap().session.unwrap().session_id, "s1");
        assert!(
            state.end(VoiceEndReason::HostDisconnected).is_none(),
            "a second end must not request a second teardown"
        );
        assert_eq!(
            state.ended_reason,
            Some(VoiceEndReason::UserRequested),
            "the first reason wins; a later end cannot rewrite history"
        );
    }

    #[test]
    fn in_flight_callbacks_cannot_mutate_a_session_that_has_ended() {
        let (mut state, generation) = live();
        state.end(VoiceEndReason::FocusedAgentChanged);
        assert!(
            !state.set_activity(generation, VoiceActivity::AgentSpeaking),
            "a state update issued before teardown must not apply afterwards"
        );
        assert!(!state.set_tool_notice(generation, Some("stale tool".to_owned())));
        assert!(!state.push_progress(
            generation,
            VoiceProgressLine {
                sequence: 99,
                text: "stale".to_owned()
            }
        ));
        assert_eq!(state.tool_notice, None);
        assert!(state.progress.is_empty());
    }

    #[test]
    fn a_stale_generation_cannot_drive_a_newer_session() {
        let mut state = VoiceUiState::default();
        let first = state.begin(target("a", "/agent/a"), key("s1"));
        state.end(VoiceEndReason::UserRequested);
        let second = state.begin(target("b", "/agent/b"), key("s2"));
        assert_ne!(first, second);
        assert!(
            !state.mic_granted(first, EffectiveAudio::default()),
            "the previous session's permission grant must not advance the new one"
        );
        assert_eq!(state.phase, VoicePhase::RequestingMic);
        assert!(state.mic_granted(second, EffectiveAudio::default()));
        assert_eq!(state.phase, VoicePhase::Negotiating);
    }

    #[test]
    fn beginning_a_session_clears_the_previous_sessions_display_state() {
        let (mut state, generation) = live();
        state.set_tool_notice(generation, Some("old tool".to_owned()));
        state.push_progress(
            generation,
            VoiceProgressLine {
                sequence: 3,
                text: "old work".to_owned(),
            },
        );
        state.end(VoiceEndReason::UserRequested);
        state.begin(target("b", "/agent/b"), key("s2"));
        assert_eq!(state.tool_notice, None);
        assert!(state.progress.is_empty());
        assert_eq!(state.last_progress_sequence, 0);
        assert_eq!(state.ended_reason, None);
        assert_eq!(state.failure, None);
    }

    #[test]
    fn phase_gates_reject_updates_that_arrive_out_of_order() {
        let mut state = VoiceUiState::default();
        let generation = state.begin(target("a", "/agent/a"), key("s1"));
        // Not live yet: progress and tool notices have nowhere to go.
        assert!(!state.set_activity(generation, VoiceActivity::AgentSpeaking));
        assert!(!state.push_progress(
            generation,
            VoiceProgressLine {
                sequence: 1,
                text: "too early".to_owned()
            }
        ));
        // `connected` requires the offer to have been negotiated first.
        assert!(!state.connected(generation));
        assert_eq!(state.phase, VoicePhase::RequestingMic);
    }

    #[test]
    fn progress_drops_replays_and_keeps_only_the_newest_lines() {
        let (mut state, generation) = live();
        for sequence in 1..=6 {
            assert!(state.push_progress(
                generation,
                VoiceProgressLine {
                    sequence,
                    text: format!("step {sequence}"),
                }
            ));
        }
        assert!(
            !state.push_progress(
                generation,
                VoiceProgressLine {
                    sequence: 4,
                    text: "replayed".to_owned()
                }
            ),
            "an already-seen sequence must not be spoken or shown twice"
        );
        assert_eq!(state.progress.len(), MAX_PROGRESS_LINES);
        assert_eq!(state.progress.first().unwrap().sequence, 3);
        assert_eq!(state.progress.last().unwrap().sequence, 6);
    }

    #[test]
    fn binding_compares_the_instance_stream_not_just_the_agent_id() {
        let (state, _) = live();
        let reconnected = target("a", "/agent/a-generation-2");
        assert!(state.is_bound_to(&target("a", "/agent/a")));
        assert!(
            !state.is_bound_to(&reconnected),
            "a new connection generation is a different target"
        );
    }

    #[test]
    fn ending_an_idle_state_is_a_no_op() {
        let mut state = VoiceUiState::default();
        assert!(state.end(VoiceEndReason::WindowHidden).is_none());
        assert_eq!(state.phase, VoicePhase::Idle);
        assert_eq!(state.ended_reason, None);
    }

    #[test]
    fn failure_keeps_the_real_message_and_dismiss_clears_it() {
        let (mut state, generation) = live();
        let ended = state.end(VoiceEndReason::MediaFailed).expect("first end");
        assert!(
            state.fail(
                ended.generation,
                VoiceFailure {
                    stage: VoiceStage::Media,
                    message: "The audio track ended unexpectedly".to_owned(),
                    retryable: true,
                }
            ),
            "the failure that caused teardown is recorded against the session that ended"
        );
        assert_eq!(state.phase, VoicePhase::Failed);
        assert_eq!(
            state.failure.as_ref().map(|f| f.message.as_str()),
            Some("The audio track ended unexpectedly")
        );
        assert!(state.dismiss());
        assert_eq!(state.phase, VoicePhase::Idle);
        assert_eq!(state.failure, None);
        assert!(!state.dismiss(), "dismissing an idle strip changes nothing");
        // A failure from an older session can never reappear.
        assert!(!state.fail(
            generation,
            VoiceFailure {
                stage: VoiceStage::Server,
                message: "stale".to_owned(),
                retryable: false,
            }
        ));
    }

    #[test]
    fn mute_is_allowed_while_engaged_and_ignored_once_ended() {
        let (mut state, generation) = live();
        assert!(state.set_muted(generation, true));
        assert!(state.muted);
        assert!(
            !state.set_muted(generation, true),
            "no-op mute is not a change"
        );
        let ended = state.end(VoiceEndReason::UserRequested).expect("end");
        assert!(!state.set_muted(ended.generation, false));
    }

    #[test]
    fn every_end_reason_has_visible_text_and_involuntary_ends_are_marked() {
        let reasons = [
            VoiceEndReason::UserRequested,
            VoiceEndReason::FocusedAgentChanged,
            VoiceEndReason::AgentGone,
            VoiceEndReason::AgentFatal,
            VoiceEndReason::InstanceStreamChanged,
            VoiceEndReason::HostDisconnected,
            VoiceEndReason::WindowHidden,
            VoiceEndReason::PageTeardown,
            VoiceEndReason::ServerEnded,
            VoiceEndReason::TransportFailed,
            VoiceEndReason::MediaFailed,
            VoiceEndReason::PermissionDenied,
        ];
        for reason in reasons {
            assert!(!reason.label().is_empty(), "{reason:?} needs visible text");
        }
        assert!(!VoiceEndReason::UserRequested.is_involuntary());
        assert!(VoiceEndReason::FocusedAgentChanged.is_involuntary());
        assert!(VoiceEndReason::WindowHidden.is_involuntary());
    }

    #[test]
    fn every_unavailable_reason_renders_a_sentence() {
        let reasons = [
            VoiceUnavailable::NoAgent,
            VoiceUnavailable::AgentNotStarted,
            VoiceUnavailable::AgentTerminated,
            VoiceUnavailable::HostDisconnected,
            VoiceUnavailable::HostReported("No reachable audio route to this host".to_owned()),
            VoiceUnavailable::BusyElsewhere,
        ];
        for reason in reasons {
            assert!(!reason.reason().is_empty(), "{reason:?} needs visible text");
        }
        assert_eq!(
            VoiceUnavailable::HostReported("custom".to_owned()).reason(),
            "custom",
            "a server-declared reason is shown verbatim, not replaced"
        );
    }
}
