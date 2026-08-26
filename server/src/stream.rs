use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use protocol::{Envelope, FrameKind, ProtocolFrame, StreamPath, VoiceAudioPayload};
use tokio::sync::Notify;

const CONTROL_LIMIT: usize = 64;
const CHAT_LIMIT: usize = 256;
const BULK_LIMIT: usize = 256;
const AUDIO_PACKET_LIMIT: usize = 8;
const CONTROL_BYTE_LIMIT: usize = 1024 * 1024;
const CHAT_BYTE_LIMIT: usize = 8 * 1024 * 1024;
const BULK_BYTE_LIMIT: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutputLane {
    Control,
    Chat,
    Bulk,
    Audio,
}

#[derive(Debug, Clone)]
pub(crate) struct QueuedOutput {
    pub frame: ProtocolFrame,
    pub lane: OutputLane,
    pub audio_packets: usize,
    pub bytes: usize,
    prerequisites: Vec<SchedulerToken>,
    pub(crate) completions: Vec<SchedulerToken>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum SchedulerToken {
    Registered(StreamPath),
    Bootstrapped(StreamPath),
}

#[derive(Default)]
struct Queues {
    control: VecDeque<QueuedOutput>,
    chat: VecDeque<QueuedOutput>,
    bulk: VecDeque<QueuedOutput>,
    audio: VecDeque<QueuedOutput>,
    audio_packets: usize,
    control_bytes: usize,
    chat_bytes: usize,
    bulk_bytes: usize,
    fatal_overflow: bool,
    closed: bool,
    audio_streak: u8,
    prefer_bulk: bool,
    streams: usize,
    close_on_no_streams: bool,
    pending_tokens: HashMap<SchedulerToken, usize>,
}

#[derive(Clone, Default)]
pub(crate) struct OutputQueue {
    inner: Arc<Mutex<Queues>>,
    ready: Arc<Notify>,
    records_written: Arc<AtomicU64>,
    dropped_audio_packets: Arc<AtomicU64>,
    dropped_audio_bytes: Arc<AtomicU64>,
    audio_high_water_packets: Arc<AtomicU64>,
}

impl std::fmt::Debug for OutputQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutputQueue").finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct StreamClosed;

#[derive(Debug)]
pub(crate) struct OutputReceiver {
    queue: OutputQueue,
}

pub(crate) fn output_channel() -> (OutputQueue, OutputReceiver) {
    let queue = OutputQueue::default();
    queue.set_close_on_no_streams();
    (queue.clone(), OutputReceiver { queue })
}

impl OutputReceiver {
    pub async fn recv(&mut self) -> Option<Envelope> {
        let output = self.queue.pop().await.ok().flatten()?;
        self.queue.complete(&output);
        Some(output.frame.envelope)
    }

    pub fn try_recv(&mut self) -> Result<Envelope, tokio::sync::mpsc::error::TryRecvError> {
        match self.queue.try_pop() {
            Ok(Some(output)) => {
                self.queue.complete(&output);
                Ok(output.frame.envelope)
            }
            Ok(None) if self.queue.is_closed() => {
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
            }
            Ok(None) => Err(tokio::sync::mpsc::error::TryRecvError::Empty),
            Err(StreamClosed) => Err(tokio::sync::mpsc::error::TryRecvError::Disconnected),
        }
    }
}

impl Drop for OutputReceiver {
    fn drop(&mut self) {
        self.queue.close();
    }
}

impl OutputQueue {
    fn queue_mut(queues: &mut Queues, lane: OutputLane) -> &mut VecDeque<QueuedOutput> {
        match lane {
            OutputLane::Control => &mut queues.control,
            OutputLane::Chat => &mut queues.chat,
            OutputLane::Bulk => &mut queues.bulk,
            OutputLane::Audio => &mut queues.audio,
        }
    }

    fn queue(queues: &Queues, lane: OutputLane) -> &VecDeque<QueuedOutput> {
        match lane {
            OutputLane::Control => &queues.control,
            OutputLane::Chat => &queues.chat,
            OutputLane::Bulk => &queues.bulk,
            OutputLane::Audio => &queues.audio,
        }
    }

    fn eligible(queues: &Queues, candidate: &QueuedOutput) -> bool {
        candidate
            .prerequisites
            .iter()
            .all(|token| !queues.pending_tokens.contains_key(token))
    }

    fn lane_bytes(queues: &Queues, lane: OutputLane) -> usize {
        match lane {
            OutputLane::Control => queues.control_bytes,
            OutputLane::Chat => queues.chat_bytes,
            OutputLane::Bulk => queues.bulk_bytes,
            OutputLane::Audio => 0,
        }
    }
    fn add_lane_bytes(queues: &mut Queues, lane: OutputLane, bytes: usize) {
        match lane {
            OutputLane::Control => queues.control_bytes += bytes,
            OutputLane::Chat => queues.chat_bytes += bytes,
            OutputLane::Bulk => queues.bulk_bytes += bytes,
            OutputLane::Audio => {}
        }
    }
    fn subtract_lane_bytes(queues: &mut Queues, item: &QueuedOutput) {
        match item.lane {
            OutputLane::Control => {
                queues.control_bytes = queues.control_bytes.saturating_sub(item.bytes)
            }
            OutputLane::Chat => queues.chat_bytes = queues.chat_bytes.saturating_sub(item.bytes),
            OutputLane::Bulk => queues.bulk_bytes = queues.bulk_bytes.saturating_sub(item.bytes),
            OutputLane::Audio => {}
        }
    }
    fn pop_lane(queues: &mut Queues, lane: OutputLane) -> Option<QueuedOutput> {
        let position = Self::queue(queues, lane)
            .iter()
            .position(|candidate| Self::eligible(queues, candidate));
        let item = position.and_then(|position| Self::queue_mut(queues, lane).remove(position));
        if let Some(item) = &item {
            if lane == OutputLane::Audio {
                queues.audio_packets = queues.audio_packets.saturating_sub(item.audio_packets)
            } else {
                Self::subtract_lane_bytes(queues, item)
            }
        }
        item
    }

    pub fn try_push(&self, output: QueuedOutput) -> Result<(), StreamClosed> {
        let mut queues = self.inner.lock().expect("output queue lock poisoned");
        if queues.closed || queues.fatal_overflow {
            return Err(StreamClosed);
        }
        if output.lane == OutputLane::Audio {
            while queues.audio_packets + output.audio_packets > AUDIO_PACKET_LIMIT {
                let Some(dropped) = queues.audio.pop_front() else {
                    break;
                };
                queues.audio_packets = queues.audio_packets.saturating_sub(dropped.audio_packets);
                self.dropped_audio_packets
                    .fetch_add(dropped.audio_packets as u64, Ordering::Relaxed);
                self.dropped_audio_bytes
                    .fetch_add(dropped.frame.binary.len() as u64, Ordering::Relaxed);
            }
            if output.audio_packets > AUDIO_PACKET_LIMIT {
                self.dropped_audio_packets
                    .fetch_add(output.audio_packets as u64, Ordering::Relaxed);
                self.dropped_audio_bytes
                    .fetch_add(output.frame.binary.len() as u64, Ordering::Relaxed);
                return Err(StreamClosed);
            }
            queues.audio_packets += output.audio_packets;
            self.audio_high_water_packets
                .fetch_max(queues.audio_packets as u64, Ordering::Relaxed);
            queues.audio.push_back(output);
            drop(queues);
            self.ready.notify_one();
            return Ok(());
        }
        let (limit, byte_limit) = match output.lane {
            OutputLane::Control => (CONTROL_LIMIT, CONTROL_BYTE_LIMIT),
            OutputLane::Chat => (CHAT_LIMIT, CHAT_BYTE_LIMIT),
            OutputLane::Bulk => (BULK_LIMIT, BULK_BYTE_LIMIT),
            OutputLane::Audio => unreachable!(),
        };
        if Self::lane_bytes(&queues, output.lane).saturating_add(output.bytes) > byte_limit {
            if output.lane == OutputLane::Control {
                queues.fatal_overflow = true;
            } else {
                queues.closed = true;
            }
            self.ready.notify_waiters();
            return Err(StreamClosed);
        }
        let lane = output.lane;
        let bytes = output.bytes;
        if Self::queue_mut(&mut queues, lane).len() >= limit {
            if lane == OutputLane::Control {
                queues.fatal_overflow = true;
            } else {
                queues.closed = true;
            }
            self.ready.notify_waiters();
            return Err(StreamClosed);
        }
        for token in &output.completions {
            *queues.pending_tokens.entry(token.clone()).or_default() += 1;
        }
        Self::queue_mut(&mut queues, lane).push_back(output);
        Self::add_lane_bytes(&mut queues, lane, bytes);
        drop(queues);
        self.ready.notify_one();
        Ok(())
    }

    pub fn complete(&self, output: &QueuedOutput) {
        self.complete_tokens(&output.completions);
    }

    pub fn complete_tokens(&self, completions: &[SchedulerToken]) {
        if completions.is_empty() {
            return;
        }
        let mut queues = self.inner.lock().expect("output queue lock poisoned");
        for token in completions {
            let remove = if let Some(count) = queues.pending_tokens.get_mut(token) {
                *count = count.saturating_sub(1);
                *count == 0
            } else {
                false
            };
            if remove {
                queues.pending_tokens.remove(token);
            }
        }
        drop(queues);
        self.ready.notify_waiters();
    }

    pub async fn pop(&self) -> Result<Option<QueuedOutput>, StreamClosed> {
        loop {
            let notified = self.ready.notified();
            {
                let mut queues = self.inner.lock().expect("output queue lock poisoned");
                if queues.fatal_overflow {
                    return Err(StreamClosed);
                }
                let item = Self::pop_fair(&mut queues, true);
                if item.is_some() {
                    return Ok(item);
                }
                if queues.closed {
                    return Ok(None);
                }
            }
            notified.await;
        }
    }

    pub fn try_pop_urgent(
        &self,
        include_audio: bool,
    ) -> Result<Option<QueuedOutput>, StreamClosed> {
        let mut queues = self.inner.lock().expect("output queue lock poisoned");
        if queues.fatal_overflow {
            return Err(StreamClosed);
        }
        if let Some(item) = Self::pop_lane(&mut queues, OutputLane::Control) {
            return Ok(Some(item));
        }
        Ok(include_audio
            .then(|| Self::pop_lane(&mut queues, OutputLane::Audio))
            .flatten())
    }

    fn try_pop(&self) -> Result<Option<QueuedOutput>, StreamClosed> {
        let mut queues = self.inner.lock().expect("output queue lock poisoned");
        if queues.fatal_overflow {
            return Err(StreamClosed);
        }
        Ok(Self::pop_fair(&mut queues, true))
    }

    fn is_closed(&self) -> bool {
        self.inner
            .lock()
            .expect("output queue lock poisoned")
            .closed
    }

    fn add_stream(&self) {
        let mut queues = self.inner.lock().expect("output queue lock poisoned");
        queues.streams = queues.streams.saturating_add(1);
    }

    fn set_close_on_no_streams(&self) {
        self.inner
            .lock()
            .expect("output queue lock poisoned")
            .close_on_no_streams = true;
    }

    fn remove_stream(&self) {
        let mut queues = self.inner.lock().expect("output queue lock poisoned");
        queues.streams = queues.streams.saturating_sub(1);
        if queues.streams == 0 && queues.close_on_no_streams {
            queues.closed = true;
            drop(queues);
            self.ready.notify_waiters();
        }
    }

    fn pop_fair(queues: &mut Queues, include_bulk: bool) -> Option<QueuedOutput> {
        if let Some(item) = Self::pop_lane(queues, OutputLane::Control) {
            return Some(item);
        }
        if queues.audio_streak >= 8 {
            let lower = Self::pop_lower(queues, include_bulk);
            if lower.is_some() {
                queues.audio_streak = 0;
                queues.prefer_bulk = !queues.prefer_bulk;
                return lower;
            }
        }
        if let Some(item) = Self::pop_lane(queues, OutputLane::Audio) {
            queues.audio_streak = queues.audio_streak.saturating_add(1);
            return Some(item);
        }
        let lower = Self::pop_lower(queues, include_bulk);
        if lower.is_some() {
            queues.audio_streak = 0;
            queues.prefer_bulk = !queues.prefer_bulk
        }
        lower
    }

    fn pop_lower(queues: &mut Queues, include_bulk: bool) -> Option<QueuedOutput> {
        if queues.prefer_bulk && include_bulk {
            if let Some(item) = Self::pop_lane(queues, OutputLane::Bulk) {
                return Some(item);
            }
            return Self::pop_lane(queues, OutputLane::Chat);
        }
        if let Some(item) = Self::pop_lane(queues, OutputLane::Chat) {
            return Some(item);
        }
        if include_bulk {
            Self::pop_lane(queues, OutputLane::Bulk)
        } else {
            None
        }
    }

    pub fn discard_voice_audio(
        &self,
        session_id: &protocol::VoiceSessionId,
        generation: u64,
    ) -> (u64, u64) {
        let mut queues = self.inner.lock().expect("output queue lock poisoned");
        let mut dropped_packets = 0u64;
        let mut dropped_bytes = 0u64;
        let audio = std::mem::take(&mut queues.audio);
        queues.audio = audio
            .into_iter()
            .filter(|item| {
                let stale = item
                    .frame
                    .envelope
                    .parse_payload::<VoiceAudioPayload>()
                    .is_ok_and(|payload| {
                        payload.session_id == *session_id && payload.generation == generation
                    });
                if stale {
                    dropped_packets += item.audio_packets as u64;
                    dropped_bytes += item.frame.binary.len() as u64;
                    queues.audio_packets = queues.audio_packets.saturating_sub(item.audio_packets);
                }
                !stale
            })
            .collect();
        (dropped_packets, dropped_bytes)
    }

    pub fn close(&self) {
        let mut queues = self.inner.lock().expect("output queue lock poisoned");
        queues.closed = true;
        queues.audio.clear();
        queues.audio_packets = 0;
        drop(queues);
        self.ready.notify_waiters();
    }

    pub fn record_written(&self) {
        self.records_written.fetch_add(1, Ordering::Relaxed);
    }
    pub fn records_written(&self) -> u64 {
        self.records_written.load(Ordering::Relaxed)
    }
    pub fn audio_metrics(&self) -> (u64, u64, u8) {
        (
            self.dropped_audio_packets.load(Ordering::Relaxed),
            self.dropped_audio_bytes.load(Ordering::Relaxed),
            self.audio_high_water_packets
                .load(Ordering::Relaxed)
                .min(u64::from(u8::MAX)) as u8,
        )
    }
}

#[derive(Debug)]
pub(crate) struct Stream {
    path: StreamPath,
    queue: OutputQueue,
}

impl Stream {
    pub fn new(path: StreamPath, queue: OutputQueue) -> Self {
        queue.add_stream();
        Self { path, queue }
    }
    pub fn with_path(&self, path: StreamPath) -> Self {
        Self::new(path, self.queue.clone())
    }
    pub fn path(&self) -> &StreamPath {
        &self.path
    }
    pub fn discard_voice_audio(
        &self,
        session_id: &protocol::VoiceSessionId,
        generation: u64,
    ) -> (u64, u64) {
        self.queue.discard_voice_audio(session_id, generation)
    }
    pub fn audio_metrics(&self) -> (u64, u64, u8) {
        self.queue.audio_metrics()
    }

    pub fn send_value(
        &self,
        kind: FrameKind,
        payload: serde_json::Value,
    ) -> Result<(), StreamClosed> {
        self.send_frame(kind, payload, Vec::new())
    }

    pub fn send_binary(
        &self,
        kind: FrameKind,
        payload: serde_json::Value,
        binary: Vec<u8>,
    ) -> Result<(), StreamClosed> {
        self.send_frame(kind, payload, binary)
    }

    fn send_frame(
        &self,
        kind: FrameKind,
        payload: serde_json::Value,
        binary: Vec<u8>,
    ) -> Result<(), StreamClosed> {
        let envelope = Envelope {
            stream: self.path.clone(),
            kind,
            seq: 0,
            payload,
        };
        let lane = classify_envelope(&envelope);
        let audio_packets = if lane == OutputLane::Audio {
            let audio = envelope
                .parse_payload::<VoiceAudioPayload>()
                .map_err(|_| StreamClosed)?;
            audio
                .validate_body(binary.len())
                .map_err(|_| StreamClosed)?;
            if audio.direction != protocol::VoiceDirection::Output {
                return Err(StreamClosed);
            }
            audio.packet_lengths.len()
        } else {
            0
        };
        let bytes = serde_json::to_vec(&envelope)
            .map_err(|_| StreamClosed)?
            .len()
            .saturating_add(binary.len());
        let (prerequisites, completions) = scheduler_metadata(&envelope);
        self.queue.try_push(QueuedOutput {
            frame: ProtocolFrame { envelope, binary },
            lane,
            audio_packets,
            bytes,
            prerequisites,
            completions,
        })
    }
}

fn classify_envelope(envelope: &Envelope) -> OutputLane {
    if envelope.kind == FrameKind::AgentCompactNotify
        && envelope
            .parse_payload::<protocol::AgentCompactNotifyPayload>()
            .is_ok_and(|payload| payload.status == protocol::AgentCompactStatus::Completed)
    {
        return OutputLane::Chat;
    }
    if envelope.kind == FrameKind::AgentError
        && envelope
            .parse_payload::<protocol::AgentErrorPayload>()
            .is_ok_and(|payload| payload.fatal)
    {
        return OutputLane::Chat;
    }
    classify(envelope.kind)
}

fn scheduler_metadata(envelope: &Envelope) -> (Vec<SchedulerToken>, Vec<SchedulerToken>) {
    let stream = envelope.stream.clone();
    let is_bootstrap = matches!(
        envelope.kind,
        FrameKind::HostBootstrap
            | FrameKind::AgentBootstrap
            | FrameKind::ProjectBootstrap
            | FrameKind::ReviewBootstrap
            | FrameKind::BrowseBootstrap
            | FrameKind::TerminalBootstrap
    );
    let mut prerequisites = Vec::new();
    if envelope.stream.0.starts_with("/host/") && !is_bootstrap {
        prerequisites.push(SchedulerToken::Bootstrapped(stream.clone()));
    } else if envelope.stream.0.starts_with("/agent/")
        || envelope.stream.0.starts_with("/terminal/")
    {
        prerequisites.push(SchedulerToken::Registered(stream.clone()));
        if !is_bootstrap {
            prerequisites.push(SchedulerToken::Bootstrapped(stream.clone()));
        }
    } else if (envelope.stream.0.starts_with("/project/")
        || envelope.stream.0.starts_with("/review/")
        || envelope.stream.0.starts_with("/browse/"))
        && !is_bootstrap
    {
        prerequisites.push(SchedulerToken::Bootstrapped(stream.clone()));
    }
    let mut completions = Vec::new();
    if is_bootstrap {
        completions.push(SchedulerToken::Bootstrapped(stream));
    }
    if envelope.kind == FrameKind::HostBootstrap
        && let Some(agents) = envelope
            .payload
            .get("agents")
            .and_then(serde_json::Value::as_array)
    {
        completions.extend(agents.iter().filter_map(|agent| {
            agent
                .get("instance_stream")
                .and_then(serde_json::Value::as_str)
                .map(|path| SchedulerToken::Registered(StreamPath(path.to_owned())))
        }));
    }
    let registration_path = match envelope.kind {
        FrameKind::NewAgent => envelope.payload.get("instance_stream"),
        FrameKind::NewTerminal => envelope.payload.get("stream"),
        _ => None,
    };
    if let Some(path) = registration_path.and_then(serde_json::Value::as_str) {
        completions.push(SchedulerToken::Registered(StreamPath(path.to_owned())));
    }
    (prerequisites, completions)
}

impl Clone for Stream {
    fn clone(&self) -> Self {
        self.queue.add_stream();
        Self {
            path: self.path.clone(),
            queue: self.queue.clone(),
        }
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.queue.remove_stream();
    }
}

pub(crate) fn classify(kind: FrameKind) -> OutputLane {
    match kind {
        FrameKind::VoiceAudio => OutputLane::Audio,
        FrameKind::ChatEvent
        | FrameKind::AgentActivitySummary
        | FrameKind::AgentActivityStats
        | FrameKind::AgentTurnStateNotify
        | FrameKind::TaskTokenUsage
        | FrameKind::QueuedMessages
        | FrameKind::SessionList
        | FrameKind::SessionSummaryCountUpdated
        | FrameKind::ProjectNotify
        | FrameKind::CustomAgentNotify
        | FrameKind::SteeringNotify
        | FrameKind::SkillNotify
        | FrameKind::McpServerNotify
        | FrameKind::TeamNotify
        | FrameKind::TeamMemberNotify
        | FrameKind::TeamMemberBindingNotify
        | FrameKind::TeamCompactNotify
        | FrameKind::TeamContextCompactionNotify
        | FrameKind::TeamPresetCatalogNotify
        | FrameKind::TeamDraftNotify
        | FrameKind::TeamMemberShuffleSuggestionNotify
        | FrameKind::ReviewEvent
        | FrameKind::ProjectEvent
        | FrameKind::WorkflowNotify
        | FrameKind::WorkflowRunNotify
        | FrameKind::AgentClosed => OutputLane::Chat,
        FrameKind::ProjectGitDiff
        | FrameKind::ProjectGitStatus
        | FrameKind::ProjectFileContents
        | FrameKind::ProjectFileList
        | FrameKind::ProjectSearchResults
        | FrameKind::ProjectSearchComplete
        | FrameKind::CodeIntelOverview
        | FrameKind::CodeIntelStatus
        | FrameKind::CodeIntelFileModel
        | FrameKind::CodeIntelDiagnostics
        | FrameKind::CodeIntelHoverResult
        | FrameKind::CodeIntelNavigateResult
        | FrameKind::CodeIntelReferencesResults
        | FrameKind::CodeIntelReferencesComplete
        | FrameKind::ProjectGitCommitResult
        | FrameKind::SessionHistory
        | FrameKind::TerminalOutput
        | FrameKind::HostBrowseEntries
        | FrameKind::BackendConfigSchemas
        | FrameKind::BackendConfigSnapshots
        | FrameKind::SessionSchemas
        | FrameKind::SessionSettings
        | FrameKind::LaunchProfileCatalogNotify
        | FrameKind::HostBootstrap
        | FrameKind::AgentBootstrap
        | FrameKind::ProjectBootstrap
        | FrameKind::ReviewBootstrap
        | FrameKind::BrowseBootstrap
        | FrameKind::TerminalBootstrap => OutputLane::Bulk,
        FrameKind::Hello
        | FrameKind::Welcome
        | FrameKind::Reject
        | FrameKind::SettingsWrite
        | FrameKind::BackendNativeSettingsWrite
        | FrameKind::InvokeSettingsAction
        | FrameKind::SettingsWriteResult
        | FrameKind::SetAgentsViewPreferences
        | FrameKind::SetAgentsSmartViews
        | FrameKind::SetAgentTags
        | FrameKind::SetAgentPins
        | FrameKind::SetAgentGroups
        | FrameKind::SpawnAgent
        | FrameKind::LoadAgent
        | FrameKind::FetchSessionHistory
        | FrameKind::ListSessions
        | FrameKind::DeleteSession
        | FrameKind::SendMessage
        | FrameKind::EditQueuedMessage
        | FrameKind::CancelQueuedMessage
        | FrameKind::CancelBackgroundTask
        | FrameKind::SendQueuedMessageNow
        | FrameKind::SetAgentName
        | FrameKind::AgentCompact
        | FrameKind::Interrupt
        | FrameKind::CloseAgent
        | FrameKind::RunBackendSetup
        | FrameKind::ProjectCreate
        | FrameKind::ProjectRename
        | FrameKind::ProjectReorder
        | FrameKind::ProjectAddRoot
        | FrameKind::ProjectDeleteRoot
        | FrameKind::ProjectDelete
        | FrameKind::WorkbenchCreate
        | FrameKind::WorkbenchRemove
        | FrameKind::CustomAgentUpsert
        | FrameKind::CustomAgentDelete
        | FrameKind::SteeringUpsert
        | FrameKind::SteeringDelete
        | FrameKind::SkillRefresh
        | FrameKind::BackendSettingsRefresh
        | FrameKind::McpServerUpsert
        | FrameKind::McpServerDelete
        | FrameKind::TeamCreate
        | FrameKind::TeamRename
        | FrameKind::TeamDelete
        | FrameKind::TeamSetManager
        | FrameKind::TeamMemberCreate
        | FrameKind::TeamMemberUpdate
        | FrameKind::TeamMemberDelete
        | FrameKind::TeamMemberActivate
        | FrameKind::TeamCompact
        | FrameKind::TeamMemberShuffle
        | FrameKind::TeamDraftCreate
        | FrameKind::TeamDraftUpdate
        | FrameKind::TeamDraftShuffle
        | FrameKind::TeamDraftApplyTemplate
        | FrameKind::TeamDraftCommit
        | FrameKind::TeamDraftDiscard
        | FrameKind::ProjectReadDiff
        | FrameKind::ProjectReadFile
        | FrameKind::ProjectSearch
        | FrameKind::ProjectSearchCancel
        | FrameKind::ProjectAccessed
        | FrameKind::CodeIntelSubscribeFile
        | FrameKind::CodeIntelUnsubscribeFile
        | FrameKind::CodeIntelSetVisibleRange
        | FrameKind::CodeIntelHover
        | FrameKind::CodeIntelNavigate
        | FrameKind::CodeIntelFindReferences
        | FrameKind::CodeIntelCancelReferences
        | FrameKind::ProjectStageFile
        | FrameKind::ProjectStageHunk
        | FrameKind::ProjectUnstageFile
        | FrameKind::ProjectDiscardFile
        | FrameKind::ProjectGitCommit
        | FrameKind::ProjectListDir
        | FrameKind::HostBrowseStart
        | FrameKind::HostBrowseList
        | FrameKind::HostBrowseClose
        | FrameKind::TerminalCreate
        | FrameKind::TerminalSend
        | FrameKind::TerminalResize
        | FrameKind::TerminalClose
        | FrameKind::MobilePairingStart
        | FrameKind::MobilePairingCancel
        | FrameKind::MobileDeviceRevoke
        | FrameKind::MobileDeviceRename
        | FrameKind::MobilePushSubscribe
        | FrameKind::MobilePushUnsubscribe
        | FrameKind::ClientError
        | FrameKind::Heartbeat
        | FrameKind::VoiceStart
        | FrameKind::VoiceInputEnd
        | FrameKind::VoiceInterrupt
        | FrameKind::VoiceStop
        | FrameKind::SetSessionSettings
        | FrameKind::TriggerWorkflow
        | FrameKind::CancelWorkflow
        | FrameKind::WorkflowRefresh
        | FrameKind::HostSettings
        | FrameKind::AgentsViewPreferencesNotify
        | FrameKind::BackendSetup
        | FrameKind::NewAgent
        | FrameKind::AgentStart
        | FrameKind::AgentRenamed
        | FrameKind::AgentCompactNotify
        | FrameKind::ContextCompactionNotify
        | FrameKind::ContextCompactionCapability
        | FrameKind::AgentError
        | FrameKind::CodeIntelError
        | FrameKind::NewTerminal
        | FrameKind::TerminalStart
        | FrameKind::TerminalExit
        | FrameKind::TerminalError
        | FrameKind::HostBrowseOpened
        | FrameKind::HostBrowseError
        | FrameKind::CommandError
        | FrameKind::BackendCapacity
        | FrameKind::MobileAccessState
        | FrameKind::MobilePairingOffer
        | FrameKind::ReviewCreate
        | FrameKind::ReviewAction
        | FrameKind::ReviewSubscribe
        | FrameKind::HeartbeatAck
        | FrameKind::VoiceCapabilities
        | FrameKind::VoiceAccepted
        | FrameKind::VoiceTranscript
        | FrameKind::VoiceState
        | FrameKind::VoiceOutput
        | FrameKind::VoiceError => OutputLane::Control,
    }
}
