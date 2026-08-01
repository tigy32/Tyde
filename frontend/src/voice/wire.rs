//! The protocol boundary for desktop voice.
//!
//! This is the **only** module in the voice layer that names `protocol::Voice*`
//! types. Everything else — the state machine, the media seam, the UI — works
//! in frontend-local terms, so a change to the wire contract lands here and
//! nowhere else.
//!
//! Wire shape, as landed in `protocol/src/types.rs` and enforced by
//! `protocol/src/validator.rs::validate_voice_envelope`:
//!
//! ```text
//! client mints session_id  ->  stream = /voice/<session_id>
//!
//! client -> host   VoiceStart{ session_id, target, capabilities }   seq 0
//! host   -> client VoiceReady{ session_id, target, direct_connections_only, expires_after_seconds }
//! client -> host   VoiceOffer{ session_id, sdp }
//! host   -> client VoiceAnswer{ session_id, sdp }
//! both             VoiceIceCandidate{ session_id, candidates }
//! both             VoiceIceCandidatesComplete{ session_id }
//! host   -> client VoiceState{ session_id, state, progress?, caption?,
//!                               transcript?, ended_reason? }
//! host   -> client VoiceError{ session_id, code, message, fatal }
//! client -> host   VoiceStop{ session_id, reason }                  (terminal)
//! ```
//!
//! Each direction has its own sequence counter on the shared stream, exactly as
//! `/agent/<id>` already works: outbound seq is reserved by
//! [`crate::send::send_frame`], inbound seq is checked by
//! [`crate::dispatch`].

use protocol::{
    FrameKind, StreamPath, VoiceAgentProgress, VoiceAgentProgressKind, VoiceAnswerPayload,
    VoiceAudioCodec, VoiceClientCapabilities, VoiceErrorPayload, VoiceIceCandidate,
    VoiceIceCandidatePayload, VoiceIceCandidatesCompletePayload, VoiceOfferPayload,
    VoiceReadyPayload, VoiceSessionId, VoiceSessionState, VoiceStartPayload, VoiceStatePayload,
    VoiceStopPayload, VoiceStopReason, VoiceTranscript, VoiceTranscriptSpeaker,
};

use super::media::{LocalIceCandidate, RemoteIceCandidate};
use super::session::{
    TranscriptLine, TranscriptSpeaker, VoiceActivity, VoiceEndReason, VoiceFailure,
    VoiceProgressLine, VoiceSessionKey, VoiceStage, VoiceTarget,
};

/// The `/voice/<session_id>` stream a session speaks on.
pub fn voice_stream(session_id: &str) -> StreamPath {
    StreamPath(format!("/voice/{session_id}"))
}

/// Recover the session id from a `/voice/<id>` stream path. Mirrors
/// `protocol::validator::voice_session_id_from_path` so an inbound frame on a
/// malformed path is ignored here rather than mis-attributed.
pub fn session_id_from_stream(stream: &StreamPath) -> Option<&str> {
    let value = stream.0.strip_prefix("/voice/")?;
    if value.is_empty() || value.contains('/') || value.len() > 128 {
        return None;
    }
    Some(value)
}

/// What this client can do. Opus only: PCMU exists in the protocol for
/// hermetic server-side negotiation tests and is not something a browser peer
/// should advertise.
///
/// Note the consequence for the hermetic path: the server's mock answers PCMU
/// unconditionally, so `setRemoteDescription` will reject an answer with no
/// codec in common and the session ends with the browser's own message. That
/// is the correct fail-visible behaviour, and it means the mock path is not
/// evidence of Opus interop either way — see the server review's codec finding.
pub fn client_capabilities() -> VoiceClientCapabilities {
    VoiceClientCapabilities {
        audio_track: true,
        codecs: vec![VoiceAudioCodec::Opus],
        echo_cancellation_requested: true,
    }
}

pub fn start_payload(session: &VoiceSessionKey, target: &VoiceTarget) -> VoiceStartPayload {
    VoiceStartPayload {
        session_id: VoiceSessionId(session.session_id.clone()),
        target: protocol::VoiceTarget {
            agent_id: target.agent.agent_id.clone(),
            instance_stream: target.instance_stream.clone(),
        },
        capabilities: client_capabilities(),
    }
}

pub fn offer_payload(session: &VoiceSessionKey, sdp: String) -> VoiceOfferPayload {
    VoiceOfferPayload {
        session_id: VoiceSessionId(session.session_id.clone()),
        sdp,
    }
}

pub fn ice_payload(
    session: &VoiceSessionKey,
    candidates: Vec<LocalIceCandidate>,
) -> VoiceIceCandidatePayload {
    VoiceIceCandidatePayload {
        session_id: VoiceSessionId(session.session_id.clone()),
        candidates: candidates
            .into_iter()
            .map(|candidate| VoiceIceCandidate {
                candidate: candidate.candidate,
                sdp_mid: candidate.sdp_mid,
                sdp_m_line_index: candidate.sdp_m_line_index,
            })
            .collect(),
    }
}

pub fn ice_complete_payload(session: &VoiceSessionKey) -> VoiceIceCandidatesCompletePayload {
    VoiceIceCandidatesCompletePayload {
        session_id: VoiceSessionId(session.session_id.clone()),
    }
}

pub fn stop_payload(session: &VoiceSessionKey, reason: VoiceEndReason) -> VoiceStopPayload {
    VoiceStopPayload {
        session_id: VoiceSessionId(session.session_id.clone()),
        reason: stop_reason(reason),
    }
}

/// Map the client's end reason onto the protocol's stop reason.
///
/// The protocol vocabulary is coarser than the client's, which is fine — the
/// host only needs to know the category. Note that every client-side reason
/// maps to something concrete: there is no "unknown" bucket that would let a
/// teardown cause disappear on the wire.
pub fn stop_reason(reason: VoiceEndReason) -> VoiceStopReason {
    match reason {
        VoiceEndReason::UserRequested => VoiceStopReason::UserExited,
        VoiceEndReason::FocusedAgentChanged | VoiceEndReason::InstanceStreamChanged => {
            VoiceStopReason::FocusChanged
        }
        VoiceEndReason::WindowHidden | VoiceEndReason::PageTeardown => {
            VoiceStopReason::ClientBackgrounded
        }
        VoiceEndReason::PermissionDenied => VoiceStopReason::PermissionLost,
        VoiceEndReason::MediaFailed | VoiceEndReason::TransportFailed => {
            VoiceStopReason::MediaFailed
        }
        VoiceEndReason::AgentGone | VoiceEndReason::AgentFatal => VoiceStopReason::AgentClosed,
        VoiceEndReason::HostDisconnected => VoiceStopReason::ClientGone,
        VoiceEndReason::ServerEnded => VoiceStopReason::ServerShutdown,
    }
}

/// The reverse mapping, for a host-initiated `VoiceState::Ended`.
pub fn end_reason_from_stop(reason: VoiceStopReason) -> VoiceEndReason {
    match reason {
        VoiceStopReason::UserExited => VoiceEndReason::UserRequested,
        VoiceStopReason::FocusChanged => VoiceEndReason::FocusedAgentChanged,
        VoiceStopReason::ClientBackgrounded => VoiceEndReason::WindowHidden,
        VoiceStopReason::PermissionLost => VoiceEndReason::PermissionDenied,
        VoiceStopReason::MediaFailed => VoiceEndReason::MediaFailed,
        VoiceStopReason::ClientGone | VoiceStopReason::ServerShutdown => {
            VoiceEndReason::ServerEnded
        }
        VoiceStopReason::AgentClosed => VoiceEndReason::AgentGone,
        VoiceStopReason::TimedOut => VoiceEndReason::ServerEnded,
    }
}

/// Host session state → what the strip says. `Ended` has no activity of its
/// own: it is handled as a teardown, not as a label.
pub fn activity_from_state(state: VoiceSessionState) -> Option<VoiceActivity> {
    match state {
        VoiceSessionState::Negotiating => Some(VoiceActivity::Connecting),
        VoiceSessionState::Connected | VoiceSessionState::Listening => {
            Some(VoiceActivity::Listening)
        }
        VoiceSessionState::AgentWorking => Some(VoiceActivity::AgentWorking),
        VoiceSessionState::Speaking => Some(VoiceActivity::AgentSpeaking),
        VoiceSessionState::Ending => Some(VoiceActivity::Ending),
        VoiceSessionState::Ended => None,
    }
}

/// Phrase a server-identified agent event.
///
/// The server decides *that* a real event happened and *which kind* it was;
/// this only chooses the words. `source_seq` is carried through untouched so
/// the state machine can drop replays.
pub fn progress_line(progress: &VoiceAgentProgress) -> VoiceProgressLine {
    let text = match progress.source_kind {
        VoiceAgentProgressKind::ResponseStarted => "Agent started responding",
        VoiceAgentProgressKind::ToolStarted => "Agent started a tool",
        VoiceAgentProgressKind::ToolProgressed => "Tool is still running",
        VoiceAgentProgressKind::TaskListChanged => "Agent updated its task list",
        VoiceAgentProgressKind::Retrying => "Agent is retrying",
        VoiceAgentProgressKind::ResponseCompleted => "Agent finished responding",
    };
    VoiceProgressLine {
        sequence: progress.source_seq,
        text: text.to_owned(),
    }
}

/// The tool banner, when the event is about a tool. `None` for every other
/// kind, so the banner clears rather than going stale.
pub fn tool_notice(progress: &VoiceAgentProgress) -> Option<String> {
    match progress.source_kind {
        VoiceAgentProgressKind::ToolStarted => Some("Agent is running a tool".to_owned()),
        VoiceAgentProgressKind::ToolProgressed => Some("Agent is still running a tool".to_owned()),
        _ => None,
    }
}

pub fn transcript_speaker(speaker: VoiceTranscriptSpeaker) -> TranscriptSpeaker {
    match speaker {
        VoiceTranscriptSpeaker::User => TranscriptSpeaker::User,
        VoiceTranscriptSpeaker::Assistant => TranscriptSpeaker::Agent,
    }
}

/// Convert a host transcript into a display line, bounded for rendering.
///
/// The host already clamps the text; bounding again here means a server that
/// stops clamping cannot produce an unbounded row in the strip.
pub fn transcript_line(transcript: VoiceTranscript) -> TranscriptLine {
    TranscriptLine::bounded(
        transcript_speaker(transcript.speaker),
        transcript.text,
        transcript.is_final,
    )
}

pub fn failure_from_error(payload: &VoiceErrorPayload) -> VoiceFailure {
    VoiceFailure {
        stage: VoiceStage::Server,
        // The host's own message, verbatim. Never replaced with a generic line.
        message: payload.message.clone(),
        retryable: !payload.fatal,
    }
}

pub fn remote_candidates(payload: VoiceIceCandidatePayload) -> Vec<RemoteIceCandidate> {
    payload
        .candidates
        .into_iter()
        .map(|candidate| RemoteIceCandidate {
            candidate: candidate.candidate,
            sdp_mid: candidate.sdp_mid,
            sdp_m_line_index: candidate.sdp_m_line_index,
        })
        .collect()
}

/// A decoded inbound voice frame, in frontend-local terms.
#[derive(Clone, Debug)]
pub enum InboundVoice {
    Ready {
        /// The host echoes the immutable binding back. The controller checks
        /// it against its own target: a Ready for a different agent means the
        /// two sides disagree about what the session is, which is precisely
        /// what the immutable binding exists to make impossible.
        target: protocol::VoiceTarget,
        direct_connections_only: bool,
        /// The host's session lease. Zero would mean a session that is over
        /// before it starts, so the controller treats it as a protocol error
        /// rather than negotiating into it.
        expires_after_seconds: u64,
    },
    Answer {
        sdp: String,
    },
    RemoteCandidates(Vec<RemoteIceCandidate>),
    RemoteCandidatesComplete,
    State {
        activity: Option<VoiceActivity>,
        progress: Option<VoiceProgressLine>,
        /// The live line, as the host phrased it. `None` means "this frame
        /// carried no caption", never "stop showing one".
        caption: Option<String>,
        /// A typed, speaker-attributed line. The host sends this alongside
        /// `caption` for the same utterance; it is what makes the rolling
        /// transcript attributable rather than a stream of anonymous text.
        transcript: Option<TranscriptLine>,
        tool_notice: Option<String>,
        /// Present exactly when the host declared the session over.
        ended: Option<VoiceEndReason>,
    },
    Error {
        failure: VoiceFailure,
        fatal: bool,
    },
}

/// Decode an inbound `/voice/<id>` envelope.
///
/// Returns `Err` with a human-readable reason for a payload that will not
/// parse; the caller logs it and ends the session rather than continuing on a
/// frame it did not understand.
pub fn decode(
    kind: FrameKind,
    envelope: &protocol::Envelope,
) -> Result<Option<InboundVoice>, String> {
    let decoded = match kind {
        FrameKind::VoiceReady => {
            let payload: VoiceReadyPayload = envelope
                .parse_payload()
                .map_err(|error| format!("failed to parse voice_ready payload: {error}"))?;
            InboundVoice::Ready {
                target: payload.target,
                direct_connections_only: payload.direct_connections_only,
                expires_after_seconds: payload.expires_after_seconds,
            }
        }
        FrameKind::VoiceAnswer => {
            let payload: VoiceAnswerPayload = envelope
                .parse_payload()
                .map_err(|error| format!("failed to parse voice_answer payload: {error}"))?;
            InboundVoice::Answer { sdp: payload.sdp }
        }
        FrameKind::VoiceIceCandidate => {
            let payload: VoiceIceCandidatePayload = envelope
                .parse_payload()
                .map_err(|error| format!("failed to parse voice_ice_candidate payload: {error}"))?;
            InboundVoice::RemoteCandidates(remote_candidates(payload))
        }
        FrameKind::VoiceIceCandidatesComplete => {
            let _: VoiceIceCandidatesCompletePayload =
                envelope.parse_payload().map_err(|error| {
                    format!("failed to parse voice_ice_candidates_complete payload: {error}")
                })?;
            InboundVoice::RemoteCandidatesComplete
        }
        FrameKind::VoiceState => {
            let payload: VoiceStatePayload = envelope
                .parse_payload()
                .map_err(|error| format!("failed to parse voice_state payload: {error}"))?;
            InboundVoice::State {
                activity: activity_from_state(payload.state),
                progress: payload.progress.as_ref().map(progress_line),
                caption: payload.caption,
                transcript: payload.transcript.map(transcript_line),
                tool_notice: payload.progress.as_ref().and_then(tool_notice),
                ended: payload.ended_reason.map(end_reason_from_stop),
            }
        }
        FrameKind::VoiceError => {
            let payload: VoiceErrorPayload = envelope
                .parse_payload()
                .map_err(|error| format!("failed to parse voice_error payload: {error}"))?;
            InboundVoice::Error {
                failure: failure_from_error(&payload),
                fatal: payload.fatal,
            }
        }
        // Frames this client sends. Seeing one inbound means the host echoed
        // our own traffic back, which is not something to act on.
        FrameKind::VoiceStart | FrameKind::VoiceOffer | FrameKind::VoiceStop => return Ok(None),
        _ => return Ok(None),
    };
    Ok(Some(decoded))
}

/// The session id an inbound voice payload claims. Used to reject a frame
/// whose body disagrees with the stream it arrived on.
pub fn payload_session_id(
    kind: FrameKind,
    envelope: &protocol::Envelope,
) -> Option<VoiceSessionId> {
    macro_rules! id_of {
        ($ty:ty) => {
            envelope
                .parse_payload::<$ty>()
                .ok()
                .map(|payload| payload.session_id)
        };
    }
    match kind {
        FrameKind::VoiceReady => id_of!(VoiceReadyPayload),
        FrameKind::VoiceAnswer => id_of!(VoiceAnswerPayload),
        FrameKind::VoiceIceCandidate => id_of!(VoiceIceCandidatePayload),
        FrameKind::VoiceIceCandidatesComplete => id_of!(VoiceIceCandidatesCompletePayload),
        FrameKind::VoiceState => id_of!(VoiceStatePayload),
        FrameKind::VoiceError => id_of!(VoiceErrorPayload),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::ActiveAgentRef;
    use protocol::AgentId;

    fn session() -> VoiceSessionKey {
        VoiceSessionKey {
            session_id: "sess-1".to_owned(),
            stream: voice_stream("sess-1"),
        }
    }

    fn target() -> VoiceTarget {
        VoiceTarget::new(
            ActiveAgentRef {
                host_id: "host-1".to_owned(),
                agent_id: AgentId("agent-1".to_owned()),
            },
            StreamPath("/agent/agent-1".to_owned()),
        )
    }

    #[test]
    fn voice_stream_and_session_id_round_trip() {
        let stream = voice_stream("abc");
        assert_eq!(stream.0, "/voice/abc");
        assert_eq!(session_id_from_stream(&stream), Some("abc"));
        assert_eq!(
            session_id_from_stream(&StreamPath("/agent/x".to_owned())),
            None
        );
        assert_eq!(
            session_id_from_stream(&StreamPath("/voice/".to_owned())),
            None
        );
        assert_eq!(
            session_id_from_stream(&StreamPath("/voice/a/b".to_owned())),
            None,
            "a nested path is not a voice session"
        );
    }

    #[test]
    fn start_payload_carries_the_immutable_target_and_an_audio_only_capability() {
        let payload = start_payload(&session(), &target());
        assert_eq!(payload.session_id.0, "sess-1");
        assert_eq!(payload.target.agent_id.0, "agent-1");
        assert_eq!(payload.target.instance_stream.0, "/agent/agent-1");
        assert!(payload.capabilities.audio_track);
        assert!(payload.capabilities.echo_cancellation_requested);
        assert_eq!(
            payload.capabilities.codecs,
            vec![VoiceAudioCodec::Opus],
            "a browser peer advertises Opus only; PCMU exists for hermetic server tests"
        );
    }

    #[test]
    fn client_frames_never_carry_audio_or_transcript_fields() {
        // The wire contract is control-only. Serializing each client frame and
        // scanning the JSON is the cheapest standing guard against someone
        // later adding an audio side channel to a signaling payload.
        let session = session();
        let frames = [
            serde_json::to_string(&start_payload(&session, &target())).unwrap(),
            serde_json::to_string(&offer_payload(&session, "v=0\r\n".to_owned())).unwrap(),
            serde_json::to_string(&ice_payload(
                &session,
                vec![LocalIceCandidate {
                    candidate: "candidate:1 1 udp".to_owned(),
                    sdp_mid: Some("0".to_owned()),
                    sdp_m_line_index: Some(0),
                }],
            ))
            .unwrap(),
            serde_json::to_string(&ice_complete_payload(&session)).unwrap(),
            serde_json::to_string(&stop_payload(&session, VoiceEndReason::UserRequested)).unwrap(),
        ];
        for frame in frames {
            for banned in ["audio_data", "pcm", "opus_frame", "samples", "transcript"] {
                assert!(
                    !frame.contains(banned),
                    "voice signaling must not carry {banned}: {frame}"
                );
            }
        }
    }

    #[test]
    fn every_client_end_reason_maps_to_a_concrete_stop_reason() {
        let reasons = [
            (VoiceEndReason::UserRequested, VoiceStopReason::UserExited),
            (
                VoiceEndReason::FocusedAgentChanged,
                VoiceStopReason::FocusChanged,
            ),
            (
                VoiceEndReason::InstanceStreamChanged,
                VoiceStopReason::FocusChanged,
            ),
            (
                VoiceEndReason::WindowHidden,
                VoiceStopReason::ClientBackgrounded,
            ),
            (
                VoiceEndReason::PageTeardown,
                VoiceStopReason::ClientBackgrounded,
            ),
            (
                VoiceEndReason::PermissionDenied,
                VoiceStopReason::PermissionLost,
            ),
            (VoiceEndReason::MediaFailed, VoiceStopReason::MediaFailed),
            (
                VoiceEndReason::TransportFailed,
                VoiceStopReason::MediaFailed,
            ),
            (VoiceEndReason::AgentGone, VoiceStopReason::AgentClosed),
            (VoiceEndReason::AgentFatal, VoiceStopReason::AgentClosed),
            (
                VoiceEndReason::HostDisconnected,
                VoiceStopReason::ClientGone,
            ),
            (VoiceEndReason::ServerEnded, VoiceStopReason::ServerShutdown),
        ];
        for (client, wire) in reasons {
            assert_eq!(stop_reason(client), wire, "{client:?} must map to {wire:?}");
        }
    }

    #[test]
    fn host_session_states_map_to_labels_and_ended_is_a_teardown_not_a_label() {
        assert_eq!(
            activity_from_state(VoiceSessionState::Negotiating),
            Some(VoiceActivity::Connecting)
        );
        assert_eq!(
            activity_from_state(VoiceSessionState::Listening),
            Some(VoiceActivity::Listening)
        );
        assert_eq!(
            activity_from_state(VoiceSessionState::AgentWorking),
            Some(VoiceActivity::AgentWorking)
        );
        assert_eq!(
            activity_from_state(VoiceSessionState::Speaking),
            Some(VoiceActivity::AgentSpeaking)
        );
        assert_eq!(
            activity_from_state(VoiceSessionState::Ended),
            None,
            "Ended must not render as an activity — it ends the session"
        );
    }

    #[test]
    fn progress_lines_carry_the_servers_source_sequence_verbatim() {
        let progress = VoiceAgentProgress {
            source_seq: 41,
            source_kind: VoiceAgentProgressKind::ToolStarted,
        };
        let line = progress_line(&progress);
        assert_eq!(
            line.sequence, 41,
            "the identity of the originating agent event must survive phrasing"
        );
        assert!(!line.text.is_empty());
        assert_eq!(
            tool_notice(&progress),
            Some("Agent is running a tool".to_owned())
        );
    }

    #[test]
    fn non_tool_progress_clears_the_tool_banner_instead_of_leaving_it_stale() {
        for kind in [
            VoiceAgentProgressKind::ResponseStarted,
            VoiceAgentProgressKind::TaskListChanged,
            VoiceAgentProgressKind::Retrying,
            VoiceAgentProgressKind::ResponseCompleted,
        ] {
            let progress = VoiceAgentProgress {
                source_seq: 1,
                source_kind: kind,
            };
            assert_eq!(tool_notice(&progress), None, "{kind:?} is not a tool event");
            assert!(
                !progress_line(&progress).text.is_empty(),
                "{kind:?} still deserves a spoken/visible line"
            );
        }
    }

    #[test]
    fn host_error_message_is_surfaced_verbatim() {
        let payload = VoiceErrorPayload {
            session_id: VoiceSessionId("sess-1".to_owned()),
            code: protocol::VoiceErrorCode::ProviderUnavailable,
            message: "Bedrock credentials could not be resolved".to_owned(),
            fatal: true,
        };
        let failure = failure_from_error(&payload);
        assert_eq!(failure.message, "Bedrock credentials could not be resolved");
        assert!(!failure.retryable, "a fatal host error is not retryable");
        assert_eq!(failure.stage, VoiceStage::Server);
    }
}
