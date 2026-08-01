use protocol::{AgentId, StreamPath, VoiceAgentProgressKind, VoiceTranscript};

use crate::state::LocalHostId;

pub const MAX_QUEUED_ICE_CANDIDATES: usize = protocol::MAX_VOICE_ICE_CANDIDATES;
pub const MAX_ICE_CANDIDATE_BYTES: usize = protocol::MAX_VOICE_ICE_CANDIDATE_BYTES;
pub const MAX_ICE_SDP_MID_BYTES: usize = 128;
pub const MAX_QUEUED_ICE_BYTES: usize =
    MAX_QUEUED_ICE_CANDIDATES * (MAX_ICE_CANDIDATE_BYTES + MAX_ICE_SDP_MID_BYTES + 16);
pub const MAX_ICE_BATCH_CANDIDATES: usize = 8;

#[derive(Clone, Debug)]
pub struct VoiceTarget {
    pub local_host_id: LocalHostId,
    pub agent_id: AgentId,
    pub instance_stream: StreamPath,
    pub agent_name: String,
}

impl PartialEq for VoiceTarget {
    fn eq(&self, other: &Self) -> bool {
        self.local_host_id == other.local_host_id
            && self.agent_id == other.agent_id
            && self.instance_stream == other.instance_stream
    }
}

impl Eq for VoiceTarget {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VoiceSession {
    pub id: String,
    pub stream: StreamPath,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BrowserAudioSetting {
    Enabled,
    Disabled,
    #[default]
    Unavailable,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AudioProcessingReport {
    pub echo_cancellation: BrowserAudioSetting,
    pub noise_suppression: BrowserAudioSetting,
    pub auto_gain_control: BrowserAudioSetting,
}

impl AudioProcessingReport {
    pub fn short_label(self) -> &'static str {
        match (
            self.echo_cancellation,
            self.noise_suppression,
            self.auto_gain_control,
        ) {
            (
                BrowserAudioSetting::Enabled,
                BrowserAudioSetting::Enabled,
                BrowserAudioSetting::Enabled,
            ) => "AEC · noise suppression · auto gain on",
            (BrowserAudioSetting::Disabled, _, _)
            | (_, BrowserAudioSetting::Disabled, _)
            | (_, _, BrowserAudioSetting::Disabled) => "Audio processing partially off",
            _ => "AEC · noise suppression · auto gain requested",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VoicePhase {
    RequestingMicrophone,
    Connecting,
    Listening,
    Working(Option<VoiceAgentProgressKind>),
    Speaking,
    Stopping,
}

impl VoicePhase {
    pub fn caption(&self) -> &str {
        match self {
            Self::RequestingMicrophone => "Allow microphone access to talk",
            Self::Connecting => "Connecting voice on your local network…",
            Self::Listening => "Listening — just speak",
            Self::Working(None) => "Working…",
            Self::Working(Some(VoiceAgentProgressKind::ResponseStarted)) => {
                "Agent response started"
            }
            Self::Working(Some(VoiceAgentProgressKind::ToolStarted)) => "Tool request started",
            Self::Working(Some(VoiceAgentProgressKind::ToolProgressed)) => {
                "Tool request in progress"
            }
            Self::Working(Some(VoiceAgentProgressKind::TaskListChanged)) => "Task progress updated",
            Self::Working(Some(VoiceAgentProgressKind::Retrying)) => "Agent is retrying…",
            Self::Working(Some(VoiceAgentProgressKind::ResponseCompleted)) => {
                "Agent response completed"
            }
            Self::Speaking => "Speaking",
            Self::Stopping => "Ending voice…",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum VoiceModel {
    #[default]
    Idle,
    Live {
        generation: u64,
        target: VoiceTarget,
        session: Option<VoiceSession>,
        phase: VoicePhase,
        muted: bool,
        processing: AudioProcessingReport,
        playback_blocked: bool,
        caption: Option<String>,
        transcript: Option<VoiceTranscript>,
    },
    Failed {
        generation: u64,
        target: Option<VoiceTarget>,
        message: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IceCandidate {
    pub candidate: String,
    pub sdp_mid: Option<String>,
    pub sdp_m_line_index: Option<u16>,
    pub username_fragment: Option<String>,
}

impl IceCandidate {
    pub fn wire_bytes(&self) -> usize {
        self.candidate.len() + self.sdp_mid.as_ref().map_or(0, String::len) + 16
    }
}

#[derive(Debug, Default)]
pub struct IceBatcher {
    queued: Vec<IceCandidate>,
    queued_bytes: usize,
    accepted_count: usize,
    complete: bool,
    completion_sent: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IceQueueError {
    CandidateTooLarge,
    MalformedCandidate,
    QueueFull,
    AlreadyComplete,
}

impl IceBatcher {
    pub fn push(&mut self, candidate: IceCandidate) -> Result<bool, IceQueueError> {
        if self.complete {
            return Err(IceQueueError::AlreadyComplete);
        }
        if !candidate.candidate.starts_with("candidate:")
            || candidate.candidate.contains('\r')
            || candidate.candidate.contains('\n')
            || candidate
                .sdp_mid
                .as_ref()
                .is_some_and(|mid| mid.len() > MAX_ICE_SDP_MID_BYTES)
        {
            return Err(IceQueueError::MalformedCandidate);
        }
        let bytes = candidate.wire_bytes();
        if self.queued.len() >= MAX_QUEUED_ICE_CANDIDATES
            || self.accepted_count >= MAX_QUEUED_ICE_CANDIDATES
        {
            return Err(IceQueueError::QueueFull);
        }
        if candidate.candidate.len() > MAX_ICE_CANDIDATE_BYTES {
            return Err(IceQueueError::CandidateTooLarge);
        }
        if self.queued_bytes.saturating_add(bytes) > MAX_QUEUED_ICE_BYTES {
            return Err(IceQueueError::QueueFull);
        }
        self.queued_bytes += bytes;
        self.accepted_count += 1;
        self.queued.push(candidate);
        Ok(self.queued.len() >= MAX_ICE_BATCH_CANDIDATES)
    }

    pub fn mark_complete(&mut self) {
        self.complete = true;
    }

    pub fn take_batch(&mut self) -> Vec<IceCandidate> {
        let take = self.queued.len().min(MAX_ICE_BATCH_CANDIDATES);
        let batch: Vec<_> = self.queued.drain(..take).collect();
        self.queued_bytes = self.queued.iter().map(IceCandidate::wire_bytes).sum();
        batch
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.queued.is_empty()
    }

    pub fn take_completion(&mut self) -> bool {
        if self.complete && !self.completion_sent && self.queued.is_empty() {
            self.completion_sent = true;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(value: &str) -> IceCandidate {
        IceCandidate {
            candidate: value.to_owned(),
            sdp_mid: Some("audio".to_owned()),
            sdp_m_line_index: Some(0),
            username_fragment: None,
        }
    }

    #[test]
    fn ice_queue_is_bounded_and_drains_in_fixed_batches() {
        let mut queue = IceBatcher::default();
        for index in 0..MAX_ICE_BATCH_CANDIDATES {
            let ready = queue
                .push(candidate(&format!("candidate:{index}")))
                .unwrap();
            assert_eq!(ready, index + 1 == MAX_ICE_BATCH_CANDIDATES);
        }
        assert_eq!(queue.take_batch().len(), MAX_ICE_BATCH_CANDIDATES);
        assert!(queue.is_empty());
        assert_eq!(
            queue.push(candidate(&format!(
                "candidate:{}",
                "x".repeat(MAX_ICE_CANDIDATE_BYTES - "candidate:".len())
            ))),
            Ok(false)
        );
        assert_eq!(
            queue.push(candidate(&format!(
                "candidate:{}",
                "x".repeat(MAX_ICE_CANDIDATE_BYTES + 1 - "candidate:".len())
            ))),
            Err(IceQueueError::CandidateTooLarge)
        );
        assert_eq!(
            queue.push(candidate("end-of-candidates")),
            Err(IceQueueError::MalformedCandidate)
        );
    }

    #[test]
    fn completed_ice_queue_rejects_late_candidates() {
        let mut queue = IceBatcher::default();
        queue.mark_complete();
        assert!(queue.take_completion());
        assert!(!queue.take_completion());
        assert_eq!(
            queue.push(candidate("late")),
            Err(IceQueueError::AlreadyComplete)
        );
    }

    #[test]
    fn cumulative_candidate_count_remains_bounded_after_batches_drain() {
        let mut queue = IceBatcher::default();
        for index in 0..MAX_QUEUED_ICE_CANDIDATES {
            queue
                .push(candidate(&format!("candidate:{index}")))
                .unwrap();
            if (index + 1) % MAX_ICE_BATCH_CANDIDATES == 0 {
                assert_eq!(queue.take_batch().len(), MAX_ICE_BATCH_CANDIDATES);
            }
        }
        assert_eq!(
            queue.push(candidate("candidate:overflow")),
            Err(IceQueueError::QueueFull)
        );
    }
}
