use std::collections::VecDeque;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc,
};
use std::thread::JoinHandle;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use serde::Serialize;
use tauri::{AppHandle, Emitter};
use webrtc_audio_processing::{
    Config, Processor,
    config::{EchoCanceller, GainController, GainController2},
};

pub const VOICE_PACKET_EVENT: &str = "tyde://voice-opus-packet";
pub const VOICE_MEDIA_EVENT: &str = "tyde://voice-media-state";

const CONTROL_QUEUE_LIMIT: usize = 16;
const MEDIA_QUEUE_LIMIT: usize = 8;
const EVENT_QUEUE_LIMIT: usize = 32;
const CONTROL_TIMEOUT: Duration = Duration::from_secs(2);
const START_TIMEOUT: Duration = Duration::from_secs(15);
const MEDIA_TIMEOUT: Duration = Duration::from_millis(250);

type CommandResult = Result<(), String>;
type Acknowledgement = mpsc::SyncSender<CommandResult>;

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct VoicePacketEvent {
    generation: u64,
    media_seq: u64,
    timestamp_samples_48k: u64,
    opus: Vec<u8>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct VoiceMediaStateEvent {
    generation: u64,
    state: &'static str,
    native_aec: bool,
}

enum AudioEvent {
    Packet(VoicePacketEvent),
    Failed { generation: u64 },
}

enum ControlCommand {
    Authorize {
        host_id: String,
        generation: u64,
        acknowledgement: Acknowledgement,
    },
    Start {
        app: Option<AppHandle>,
        host_id: String,
        generation: u64,
        acknowledgement: Acknowledgement,
    },
    Flush {
        generation: u64,
        acknowledgement: Acknowledgement,
    },
    Stop {
        acknowledgement: Acknowledgement,
    },
    StopHost {
        host_id: String,
        acknowledgement: Acknowledgement,
    },
    Shutdown {
        acknowledgement: Acknowledgement,
    },
    #[cfg(test)]
    ActivateTest {
        host_id: String,
        generation: u64,
        opened: Arc<std::sync::atomic::AtomicUsize>,
        dropped: Arc<std::sync::atomic::AtomicUsize>,
        sink: Arc<TestSinkObservation>,
        acknowledgement: Acknowledgement,
    },
    #[cfg(test)]
    BlockTest {
        entered: mpsc::SyncSender<()>,
        release: mpsc::Receiver<()>,
        acknowledgement: Acknowledgement,
    },
}

enum MediaCommand {
    Push {
        generation: u64,
        media_seq: u64,
        timestamp_samples_48k: u64,
        opus: Vec<u8>,
        acknowledgement: Acknowledgement,
    },
}

struct AudioControl {
    control_tx: mpsc::SyncSender<ControlCommand>,
    media_tx: mpsc::SyncSender<MediaCommand>,
    healthy: AtomicBool,
    thread_exited: Arc<AtomicBool>,
    join: Mutex<Option<JoinHandle<()>>>,
}

#[derive(Clone)]
pub struct NativeVoiceMedia {
    control: Arc<AudioControl>,
}

impl NativeVoiceMedia {
    pub fn new() -> Result<Self, String> {
        let (control_tx, control_rx) = mpsc::sync_channel(CONTROL_QUEUE_LIMIT);
        let (media_tx, media_rx) = mpsc::sync_channel(MEDIA_QUEUE_LIMIT);
        let (event_tx, event_rx) = mpsc::sync_channel(EVENT_QUEUE_LIMIT);
        let thread_exited = Arc::new(AtomicBool::new(false));
        let thread_exit_flag = thread_exited.clone();
        let join = std::thread::Builder::new()
            .name("tyde-native-audio".into())
            .spawn(move || {
                run_audio_thread(control_rx, media_rx, event_tx, event_rx);
                thread_exit_flag.store(true, Ordering::Release);
            })
            .map_err(|error| format!("Could not start native audio thread: {error}"))?;
        Ok(Self {
            control: Arc::new(AudioControl {
                control_tx,
                media_tx,
                healthy: AtomicBool::new(true),
                thread_exited,
                join: Mutex::new(Some(join)),
            }),
        })
    }

    pub fn start(&self, app: AppHandle, host_id: String, generation: u64) -> CommandResult {
        self.control_request(
            |acknowledgement| ControlCommand::Start {
                app: Some(app),
                host_id,
                generation,
                acknowledgement,
            },
            START_TIMEOUT,
        )
    }

    pub fn push_output(
        &self,
        generation: u64,
        media_seq: u64,
        timestamp_samples_48k: u64,
        opus: Vec<u8>,
    ) -> CommandResult {
        if opus.is_empty() || opus.len() > 1275 {
            return Err("invalid Opus packet size".into());
        }
        if !self.control.healthy.load(Ordering::Acquire)
            || self.control.thread_exited.load(Ordering::Acquire)
        {
            return Err("native audio thread is unavailable".into());
        }
        let (acknowledgement, result) = mpsc::sync_channel(1);
        self.control
            .media_tx
            .try_send(MediaCommand::Push {
                generation,
                media_seq,
                timestamp_samples_48k,
                opus,
                acknowledgement,
            })
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => "native audio media command queue full".to_owned(),
                mpsc::TrySendError::Disconnected(_) => {
                    self.control.healthy.store(false, Ordering::Release);
                    "native audio thread disconnected".to_owned()
                }
            })?;
        self.await_acknowledgement(result, MEDIA_TIMEOUT)
    }

    pub fn flush_output(&self, generation: u64) -> CommandResult {
        self.control_request(
            |acknowledgement| ControlCommand::Flush {
                generation,
                acknowledgement,
            },
            CONTROL_TIMEOUT,
        )
    }

    pub fn stop(&self) -> CommandResult {
        self.control_request(
            |acknowledgement| ControlCommand::Stop { acknowledgement },
            CONTROL_TIMEOUT,
        )
    }

    pub fn stop_for_host(&self, host_id: &str) -> CommandResult {
        let host_id = host_id.to_owned();
        self.control_request(
            |acknowledgement| ControlCommand::StopHost {
                host_id,
                acknowledgement,
            },
            CONTROL_TIMEOUT,
        )
    }

    pub fn authorize(&self, host_id: String, generation: u64) -> CommandResult {
        self.control_request(
            |acknowledgement| ControlCommand::Authorize {
                host_id,
                generation,
                acknowledgement,
            },
            CONTROL_TIMEOUT,
        )
    }

    fn control_request(
        &self,
        command: impl FnOnce(Acknowledgement) -> ControlCommand,
        timeout: Duration,
    ) -> CommandResult {
        if !self.control.healthy.load(Ordering::Acquire)
            || self.control.thread_exited.load(Ordering::Acquire)
        {
            return Err("native audio thread is unavailable".into());
        }
        let (acknowledgement, result) = mpsc::sync_channel(1);
        self.control
            .control_tx
            .try_send(command(acknowledgement))
            .map_err(|error| {
                self.control.healthy.store(false, Ordering::Release);
                match error {
                    mpsc::TrySendError::Full(_) => {
                        "native audio control queue full; media is fail-closed".to_owned()
                    }
                    mpsc::TrySendError::Disconnected(_) => {
                        "native audio thread disconnected".to_owned()
                    }
                }
            })?;
        self.await_acknowledgement(result, timeout)
    }

    fn await_acknowledgement(
        &self,
        result: mpsc::Receiver<CommandResult>,
        timeout: Duration,
    ) -> CommandResult {
        match result.recv_timeout(timeout) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.control.healthy.store(false, Ordering::Release);
                Err("native audio command acknowledgement timed out; media is fail-closed".into())
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                self.control.healthy.store(false, Ordering::Release);
                Err("native audio thread stopped before acknowledging teardown".into())
            }
        }
    }

    #[cfg(test)]
    fn activate_test(
        &self,
        host_id: &str,
        generation: u64,
        opened: Arc<std::sync::atomic::AtomicUsize>,
        dropped: Arc<std::sync::atomic::AtomicUsize>,
    ) -> CommandResult {
        self.activate_test_with_sink(
            host_id,
            generation,
            opened,
            dropped,
            Arc::new(TestSinkObservation::default()),
        )
    }

    #[cfg(test)]
    pub(super) fn activate_test_with_sink(
        &self,
        host_id: &str,
        generation: u64,
        opened: Arc<std::sync::atomic::AtomicUsize>,
        dropped: Arc<std::sync::atomic::AtomicUsize>,
        sink: Arc<TestSinkObservation>,
    ) -> CommandResult {
        let host_id = host_id.to_owned();
        self.control_request(
            |acknowledgement| ControlCommand::ActivateTest {
                host_id,
                generation,
                opened,
                dropped,
                sink,
                acknowledgement,
            },
            CONTROL_TIMEOUT,
        )
    }
}

impl Drop for AudioControl {
    fn drop(&mut self) {
        self.healthy.store(false, Ordering::Release);
        let (acknowledgement, result) = mpsc::sync_channel(1);
        let acknowledged = self
            .control_tx
            .try_send(ControlCommand::Shutdown { acknowledgement })
            .is_ok()
            && result.recv_timeout(CONTROL_TIMEOUT).is_ok();
        let join = self.join.lock().expect("native audio join lock").take();
        if let (true, Some(join)) = (acknowledged, join) {
            let _ = join.join();
        }
    }
}

struct Session {
    input: cpal::Stream,
    output: cpal::Stream,
    output_tx: mpsc::SyncSender<Vec<u8>>,
    playback_epoch: Arc<AtomicU64>,
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.input.pause();
        let _ = self.output.pause();
    }
}

#[cfg(test)]
struct TestSession {
    dropped: Arc<std::sync::atomic::AtomicUsize>,
    sink: Arc<TestSinkObservation>,
}

#[cfg(test)]
#[derive(Default)]
pub(super) struct TestSinkObservation {
    pub(super) received_packets: std::sync::atomic::AtomicUsize,
    pub(super) last_media_seq: AtomicU64,
}

enum LiveSession {
    Device(Session),
    #[cfg(test)]
    Test(TestSession),
}

#[cfg(test)]
impl Drop for LiveSession {
    fn drop(&mut self) {
        match self {
            Self::Device(_) => {}
            #[cfg(test)]
            Self::Test(session) => {
                session.dropped.fetch_add(1, Ordering::SeqCst);
            }
        }
    }
}

struct ActiveSession {
    host_id: String,
    generation: u64,
    app: Option<AppHandle>,
    media: LiveSession,
}

#[derive(Default)]
struct AudioThreadState {
    accepted: Option<(String, u64)>,
    active: Option<ActiveSession>,
}

impl AudioThreadState {
    fn consume_acceptance(&mut self, host_id: &str, generation: u64) -> CommandResult {
        let accepted = self.accepted.take();
        if accepted.as_ref() == Some(&(host_id.to_owned(), generation)) {
            Ok(())
        } else {
            Err("native microphone start was not authorized by VoiceAccepted".into())
        }
    }

    fn stop(&mut self) {
        self.accepted.take();
        drop(self.active.take());
    }

    fn stop_active(&mut self) {
        drop(self.active.take());
    }

    fn stop_for_host(&mut self, host_id: &str) {
        if self
            .accepted
            .as_ref()
            .is_some_and(|(accepted_host, _)| accepted_host == host_id)
        {
            self.accepted.take();
        }
        if self
            .active
            .as_ref()
            .is_some_and(|active| active.host_id == host_id)
        {
            drop(self.active.take());
        }
    }

    fn push_output(
        &mut self,
        generation: u64,
        _media_seq: u64,
        _timestamp_samples_48k: u64,
        opus: Vec<u8>,
    ) -> CommandResult {
        let Some(active_generation) = self.active.as_ref().map(|active| active.generation) else {
            return Err("stale voice media generation".into());
        };
        if active_generation != generation {
            return Err("stale voice media generation".into());
        }
        let session = self.active.as_mut().expect("active voice session checked");
        match &mut session.media {
            LiveSession::Device(session) => session
                .output_tx
                .try_send(opus)
                .map_err(|_| "voice output queue full".into()),
            #[cfg(test)]
            LiveSession::Test(session) => {
                session.sink.received_packets.fetch_add(1, Ordering::SeqCst);
                session
                    .sink
                    .last_media_seq
                    .store(_media_seq, Ordering::SeqCst);
                Ok(())
            }
        }
    }

    fn flush(&mut self, generation: u64) -> CommandResult {
        let session = self
            .active
            .as_mut()
            .filter(|active| active.generation == generation)
            .ok_or("stale voice media generation")?;
        match &mut session.media {
            LiveSession::Device(session) => {
                advance_playback_epoch(&session.playback_epoch);
                Ok(())
            }
            #[cfg(test)]
            LiveSession::Test(_) => Ok(()),
        }
    }
}

fn enqueue_audio_packet(event_tx: &mpsc::SyncSender<AudioEvent>, packet: VoicePacketEvent) -> bool {
    event_tx.try_send(AudioEvent::Packet(packet)).is_ok()
}

fn run_audio_thread(
    control_rx: mpsc::Receiver<ControlCommand>,
    media_rx: mpsc::Receiver<MediaCommand>,
    event_tx: mpsc::SyncSender<AudioEvent>,
    event_rx: mpsc::Receiver<AudioEvent>,
) {
    let mut state = AudioThreadState::default();
    let mut running = true;
    while running {
        while let Ok(command) = control_rx.try_recv() {
            running = handle_control_command(command, &mut state, &event_tx);
            if !running {
                break;
            }
        }
        if !running {
            break;
        }
        while let Ok(event) = event_rx.try_recv() {
            handle_audio_event(event, &mut state);
        }
        match media_rx.recv_timeout(Duration::from_millis(5)) {
            Ok(MediaCommand::Push {
                generation,
                media_seq,
                timestamp_samples_48k,
                opus,
                acknowledgement,
            }) => {
                let _ = acknowledgement.send(state.push_output(
                    generation,
                    media_seq,
                    timestamp_samples_48k,
                    opus,
                ));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                if matches!(control_rx.try_recv(), Err(mpsc::TryRecvError::Disconnected)) {
                    break;
                }
            }
        }
    }
    state.stop();
}

fn handle_control_command(
    command: ControlCommand,
    state: &mut AudioThreadState,
    event_tx: &mpsc::SyncSender<AudioEvent>,
) -> bool {
    match command {
        ControlCommand::Authorize {
            host_id,
            generation,
            acknowledgement,
        } => {
            state.accepted = Some((host_id, generation));
            let _ = acknowledgement.send(Ok(()));
        }
        ControlCommand::Start {
            app,
            host_id,
            generation,
            acknowledgement,
        } => {
            let result = state
                .consume_acceptance(&host_id, generation)
                .and_then(|()| {
                    state.stop_active();
                    open_device_session(generation, event_tx.clone()).map(LiveSession::Device)
                })
                .map(|media| {
                    state.active = Some(ActiveSession {
                        host_id,
                        generation,
                        app: app.clone(),
                        media,
                    });
                    if let Some(app) = app {
                        let _ = app.emit(
                            VOICE_MEDIA_EVENT,
                            VoiceMediaStateEvent {
                                generation,
                                state: "active",
                                native_aec: true,
                            },
                        );
                    }
                });
            if acknowledgement.send(result).is_err() {
                state.stop();
            }
        }
        ControlCommand::Flush {
            generation,
            acknowledgement,
        } => {
            let _ = acknowledgement.send(state.flush(generation));
        }
        ControlCommand::Stop { acknowledgement } => {
            state.stop();
            let _ = acknowledgement.send(Ok(()));
        }
        ControlCommand::StopHost {
            host_id,
            acknowledgement,
        } => {
            state.stop_for_host(&host_id);
            let _ = acknowledgement.send(Ok(()));
        }
        ControlCommand::Shutdown { acknowledgement } => {
            state.stop();
            let _ = acknowledgement.send(Ok(()));
            return false;
        }
        #[cfg(test)]
        ControlCommand::ActivateTest {
            host_id,
            generation,
            opened,
            dropped,
            sink,
            acknowledgement,
        } => {
            let result = state.consume_acceptance(&host_id, generation).map(|()| {
                state.stop_active();
                opened.fetch_add(1, Ordering::SeqCst);
                state.active = Some(ActiveSession {
                    host_id,
                    generation,
                    app: None,
                    media: LiveSession::Test(TestSession { dropped, sink }),
                });
            });
            let _ = acknowledgement.send(result);
        }
        #[cfg(test)]
        ControlCommand::BlockTest {
            entered,
            release,
            acknowledgement,
        } => {
            let _ = entered.send(());
            let result = release
                .recv_timeout(CONTROL_TIMEOUT)
                .map_err(|_| "test audio thread release timed out".to_owned());
            let _ = acknowledgement.send(result);
        }
    }
    true
}

fn handle_audio_event(event: AudioEvent, state: &mut AudioThreadState) {
    match event {
        AudioEvent::Packet(packet) => {
            if let Some(active) = state
                .active
                .as_ref()
                .filter(|active| active.generation == packet.generation)
                && let Some(app) = &active.app
            {
                let _ = app.emit(VOICE_PACKET_EVENT, packet);
            }
        }
        AudioEvent::Failed { generation } => {
            let failure = state
                .active
                .as_ref()
                .filter(|active| active.generation == generation)
                .and_then(|active| {
                    active.app.clone().map(|app| {
                        let native_aec = matches!(&active.media, LiveSession::Device(_));
                        (app, native_aec)
                    })
                });
            if let Some((app, native_aec)) = failure {
                let _ = app.emit(
                    VOICE_MEDIA_EVENT,
                    VoiceMediaStateEvent {
                        generation,
                        state: "failed",
                        native_aec,
                    },
                );
                state.stop_active();
            }
        }
    }
}

fn advance_playback_epoch(epoch: &AtomicU64) {
    epoch.fetch_add(1, Ordering::AcqRel);
}

fn open_device_session(
    generation: u64,
    event_tx: mpsc::SyncSender<AudioEvent>,
) -> Result<Session, String> {
    let host = cpal::default_host();
    let input_device = host
        .default_input_device()
        .ok_or("No microphone is available")?;
    let output_device = host
        .default_output_device()
        .ok_or("No audio output is available")?;
    let input_config = mono_f32_config(&input_device, true)?;
    let output_config = mono_f32_config(&output_device, false)?;
    let processor = Arc::new(
        Processor::new(48_000).map_err(|error| format!("AEC initialization failed: {error}"))?,
    );
    processor.set_config(Config {
        echo_canceller: Some(EchoCanceller::Full {
            stream_delay_ms: None,
        }),
        high_pass_filter: Some(Default::default()),
        noise_suppression: Some(Default::default()),
        gain_controller: Some(GainController::GainController2(GainController2::default())),
        ..Default::default()
    });

    let (output_tx, output_rx) = mpsc::sync_channel::<Vec<u8>>(8);
    let output_rx = Arc::new(Mutex::new(output_rx));
    let playback_epoch = Arc::new(AtomicU64::new(0));
    let callback_epoch = playback_epoch.clone();
    let mut seen_epoch = 0;
    let output_processor = processor.clone();
    let output_error_tx = event_tx.clone();
    let mut decoder = opus::Decoder::new(48_000, opus::Channels::Mono)
        .map_err(|error| format!("Opus decoder failed: {error}"))?;
    let mut playback = VecDeque::<f32>::new();
    let mut render_frame = Vec::with_capacity(480);
    let output_channels = usize::from(output_config.channels);
    let output = output_device
        .build_output_stream(
            &output_config,
            move |data: &mut [f32], _| {
                let epoch = callback_epoch.load(Ordering::Acquire);
                if epoch != seen_epoch {
                    seen_epoch = epoch;
                    playback.clear();
                    while output_rx
                        .lock()
                        .expect("voice output lock")
                        .try_recv()
                        .is_ok()
                    {}
                    render_frame.clear();
                }
                while let Ok(packet) = output_rx.lock().expect("voice output lock").try_recv() {
                    let mut pcm = vec![0i16; 960];
                    if let Ok(samples) = decoder.decode(&packet, &mut pcm, false) {
                        playback.extend(
                            pcm[..samples]
                                .iter()
                                .map(|sample| f32::from(*sample) / 32768.0),
                        );
                    }
                }
                for frame in data.chunks_mut(output_channels) {
                    let sample = playback.pop_front().unwrap_or(0.0);
                    render_frame.push(sample);
                    for channel in frame {
                        *channel = sample;
                    }
                    if render_frame.len() == 480 {
                        let mut channels = vec![std::mem::take(&mut render_frame)];
                        let _ = output_processor.process_render_frame(&mut channels);
                        render_frame = Vec::with_capacity(480);
                    }
                }
            },
            move |error| {
                tracing::warn!(code = "voice_output_device", %error, "native voice output failed");
                let _ = output_error_tx.try_send(AudioEvent::Failed { generation });
            },
            None,
        )
        .map_err(|error| format!("Could not open audio output: {error}"))?;

    let input_processor = processor;
    let input_packet_tx = event_tx.clone();
    let input_error_tx = event_tx;
    let mut capture = Vec::with_capacity(480);
    let mut opus_pcm = Vec::with_capacity(960);
    let mut encoder = opus::Encoder::new(48_000, opus::Channels::Mono, opus::Application::Voip)
        .map_err(|error| format!("Opus encoder failed: {error}"))?;
    encoder
        .set_bitrate(opus::Bitrate::Bits(24_000))
        .map_err(|error| format!("Opus bitrate failed: {error}"))?;
    let mut media_seq = 0u64;
    let input_channels = usize::from(input_config.channels);
    let input = input_device
        .build_input_stream(
            &input_config,
            move |data: &[f32], _| {
                for frame in data.chunks(input_channels) {
                    capture.push(frame.iter().copied().sum::<f32>() / frame.len().max(1) as f32);
                    if capture.len() == 480 {
                        let mut channels = vec![std::mem::take(&mut capture)];
                        if input_processor.process_capture_frame(&mut channels).is_ok() {
                            opus_pcm.extend(
                                channels[0]
                                    .iter()
                                    .map(|sample| (sample.clamp(-1.0, 1.0) * 32767.0) as i16),
                            );
                        }
                        capture = Vec::with_capacity(480);
                        if opus_pcm.len() == 960 {
                            let mut opus = vec![0; 1275];
                            if let Ok(len) = encoder.encode(&opus_pcm, &mut opus) {
                                opus.truncate(len);
                                let packet = VoicePacketEvent {
                                    generation,
                                    media_seq,
                                    timestamp_samples_48k: media_seq * 960,
                                    opus,
                                };
                                let _ = enqueue_audio_packet(&input_packet_tx, packet);
                                media_seq = media_seq.saturating_add(1);
                            }
                            opus_pcm.clear();
                        }
                    }
                }
            },
            move |error| {
                tracing::warn!(code = "voice_input_device", %error, "native voice input failed");
                let _ = input_error_tx.try_send(AudioEvent::Failed { generation });
            },
            None,
        )
        .map_err(|error| format!("Could not open microphone: {error}"))?;
    output
        .play()
        .map_err(|error| format!("Could not start output: {error}"))?;
    input
        .play()
        .map_err(|error| format!("Could not start microphone: {error}"))?;
    Ok(Session {
        input,
        output,
        output_tx,
        playback_epoch,
    })
}

fn mono_f32_config(device: &cpal::Device, input: bool) -> Result<cpal::StreamConfig, String> {
    let ranges: Vec<_> = if input {
        device
            .supported_input_configs()
            .map_err(|error| error.to_string())?
            .collect()
    } else {
        device
            .supported_output_configs()
            .map_err(|error| error.to_string())?
            .collect()
    };
    ranges
        .into_iter()
        .find(|range| {
            range.sample_format() == cpal::SampleFormat::F32
                && range.min_sample_rate().0 <= 48_000
                && range.max_sample_rate().0 >= 48_000
        })
        .map(|range| {
            let mut config = range.with_sample_rate(cpal::SampleRate(48_000)).config();
            config.buffer_size = cpal::BufferSize::Default;
            config
        })
        .ok_or_else(|| {
            if input {
                // macOS hides input formats from processes without microphone
                // permission, so a missing 48 kHz format on a Mac with a
                // working microphone usually means the permission was never
                // granted (or never requestable for this binary).
                "The microphone offers no 48 kHz f32 format. If this machine has a working \
                 microphone, Tyde likely lacks microphone permission (macOS: System Settings \
                 → Privacy & Security → Microphone)."
                    .into()
            } else {
                "The audio output device offers no 48 kHz f32 format".into()
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn shell_handle_is_send_sync_without_audio_resources() {
        assert_send_sync::<NativeVoiceMedia>();
    }

    #[test]
    fn threaded_lifecycle_requires_acceptance_and_acknowledges_fresh_drop() {
        let media = NativeVoiceMedia::new().unwrap();
        let opened = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicUsize::new(0));
        assert!(
            media
                .activate_test("host-a", 1, opened.clone(), dropped.clone())
                .is_err()
        );
        assert_eq!(opened.load(Ordering::SeqCst), 0);

        media.authorize("host-a".into(), 1).unwrap();
        assert!(
            media
                .activate_test("host-b", 1, opened.clone(), dropped.clone())
                .is_err()
        );
        assert!(
            media
                .activate_test("host-a", 1, opened.clone(), dropped.clone())
                .is_err()
        );
        assert_eq!(opened.load(Ordering::SeqCst), 0);

        media.authorize("host-a".into(), 1).unwrap();
        media
            .activate_test("host-a", 1, opened.clone(), dropped.clone())
            .unwrap();
        assert_eq!(opened.load(Ordering::SeqCst), 1);
        assert_eq!(dropped.load(Ordering::SeqCst), 0);

        media.authorize("host-a".into(), 2).unwrap();
        media
            .activate_test("host-a", 2, opened.clone(), dropped.clone())
            .unwrap();
        assert_eq!(opened.load(Ordering::SeqCst), 2);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);

        media.stop_for_host("host-a").unwrap();
        assert_eq!(dropped.load(Ordering::SeqCst), 2);
        media.authorize("host-b".into(), 3).unwrap();
        media.stop().unwrap();
        assert!(media.activate_test("host-b", 3, opened, dropped).is_err());
    }

    #[test]
    fn dropping_control_handle_shuts_thread_and_drops_session() {
        let media = NativeVoiceMedia::new().unwrap();
        let opened = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicUsize::new(0));
        media.authorize("host-a".into(), 1).unwrap();
        media
            .activate_test("host-a", 1, opened, dropped.clone())
            .unwrap();
        let thread_exited = media.control.thread_exited.clone();
        drop(media);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        assert!(thread_exited.load(Ordering::Acquire));
    }

    #[test]
    fn media_command_queue_is_bounded_under_thread_backpressure() {
        let media = NativeVoiceMedia::new().unwrap();
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let (block_ack, block_result) = mpsc::sync_channel(1);
        media
            .control
            .control_tx
            .try_send(ControlCommand::BlockTest {
                entered: entered_tx,
                release: release_rx,
                acknowledgement: block_ack,
            })
            .unwrap();
        entered_rx.recv_timeout(CONTROL_TIMEOUT).unwrap();

        let mut acknowledgements = Vec::new();
        for _ in 0..MEDIA_QUEUE_LIMIT {
            let (acknowledgement, result) = mpsc::sync_channel(1);
            media
                .control
                .media_tx
                .try_send(MediaCommand::Push {
                    generation: 1,
                    media_seq: 0,
                    timestamp_samples_48k: 0,
                    opus: vec![1],
                    acknowledgement,
                })
                .unwrap();
            acknowledgements.push(result);
        }
        assert_eq!(
            media.push_output(1, 0, 0, vec![1]).unwrap_err(),
            "native audio media command queue full"
        );
        assert!(media.control.healthy.load(Ordering::Acquire));
        release_tx.send(()).unwrap();
        block_result.recv_timeout(CONTROL_TIMEOUT).unwrap().unwrap();
        drop(acknowledgements);
    }

    #[test]
    fn interrupt_barrier_invalidates_native_playback_callback_epoch() {
        let epoch = AtomicU64::new(4);
        advance_playback_epoch(&epoch);
        assert_eq!(epoch.load(Ordering::Acquire), 5);
    }

    #[test]
    fn push_output_rejects_mismatched_generation_before_playback() {
        let media = NativeVoiceMedia::new().unwrap();
        let opened = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicUsize::new(0));
        let sink = Arc::new(TestSinkObservation::default());
        media.authorize("host-a".into(), 7).unwrap();
        media
            .activate_test_with_sink("host-a", 7, opened, dropped, sink.clone())
            .unwrap();

        media.push_output(7, 4, 3_840, vec![1, 2, 3]).unwrap();
        assert_eq!(sink.received_packets.load(Ordering::SeqCst), 1);
        assert_eq!(sink.last_media_seq.load(Ordering::SeqCst), 4);

        assert!(media.push_output(8, 5, 4_800, vec![4, 5, 6]).is_err());
        assert_eq!(sink.received_packets.load(Ordering::SeqCst), 1);
        assert_eq!(sink.last_media_seq.load(Ordering::SeqCst), 4);
        media.stop().unwrap();
    }

    #[test]
    fn native_processor_performs_real_echo_cancellation() {
        let processor = Processor::new(48_000).expect("create native AEC processor");
        processor.set_config(Config {
            echo_canceller: Some(EchoCanceller::default()),
            ..Default::default()
        });
        let render: Vec<f32> = (0..480)
            .map(|sample| ((sample as f32) * 0.07).sin() * 0.25)
            .collect();
        let mut changed_capture = false;
        for _ in 0..20 {
            let mut render_channels = vec![render.clone()];
            let mut capture_channels = vec![render.clone()];
            processor
                .process_render_frame(&mut render_channels)
                .expect("submit render reference");
            processor
                .process_capture_frame(&mut capture_channels)
                .expect("process captured echo");
            changed_capture |= capture_channels[0] != render;
        }
        assert!(changed_capture, "AEC must alter a captured render echo");
        assert!(processor.get_stats().echo_return_loss.is_some());
    }
}
