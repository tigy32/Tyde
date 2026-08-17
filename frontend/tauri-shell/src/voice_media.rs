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

enum LiveSession {
    Device(Session),
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
    let (input_config, input_rate) = device_f32_config(&input_device, true)?;
    let (output_config, output_rate) = device_f32_config(&output_device, false)?;
    if input_rate != 48_000 || output_rate != 48_000 {
        tracing::info!(
            input_rate,
            output_rate,
            "voice devices run off 48 kHz; resampling engaged"
        );
    }
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

    // Bedrock streams a response's audio several times faster than real-time,
    // so this queue must absorb the burst between IPC pushes and the output
    // callback's drain — at 8 slots (160ms) it overflowed on every response
    // and dropped packets mid-word. 1024 packets ≈ 20s of 20ms Opus frames
    // (~60KB), beyond any single response burst, while still bounded so a
    // wedged callback fails visibly instead of accumulating forever.
    let (output_tx, output_rx) = mpsc::sync_channel::<Vec<u8>>(1024);
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
    let mut output_resampler = Resampler::new(48_000, output_rate);
    let mut device_ready = VecDeque::<f32>::new();
    let mut jitter_gate = JitterGate::new(JITTER_TARGET_SAMPLES);
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
                    output_resampler.reset();
                    device_ready.clear();
                    jitter_gate.reset();
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
                // The playback clock and AEC render reference stay in the
                // 48 kHz domain even when the device runs at another rate;
                // an underrun renders silence, exactly as it did at 48 kHz.
                let frames_needed = data.len() / output_channels.max(1);
                while device_ready.len() < frames_needed {
                    let sample = jitter_gate.next(&mut playback);
                    render_frame.push(sample);
                    if render_frame.len() == 480 {
                        let mut channels = vec![std::mem::take(&mut render_frame)];
                        let _ = output_processor.process_render_frame(&mut channels);
                        render_frame = Vec::with_capacity(480);
                    }
                    output_resampler.push(sample, &mut device_ready);
                }
                for frame in data.chunks_mut(output_channels) {
                    let sample = device_ready.pop_front().unwrap_or(0.0);
                    for channel in frame {
                        *channel = sample;
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
    let mut input_resampler = Resampler::new(input_rate, 48_000);
    let mut resampled = VecDeque::<f32>::new();
    let input_channels = usize::from(input_config.channels);
    let input = input_device
        .build_input_stream(
            &input_config,
            move |data: &[f32], _| {
                for frame in data.chunks(input_channels) {
                    let mono = frame.iter().copied().sum::<f32>() / frame.len().max(1) as f32;
                    input_resampler.push(mono, &mut resampled);
                }
                while let Some(sample) = resampled.pop_front() {
                    capture.push(sample);
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

/// AEC and Opus run at a fixed 48 kHz, but the default devices may not:
/// Bluetooth headsets in call mode commonly expose only 8/16/24 kHz. Device
/// streams therefore open at the closest rate the hardware offers and are
/// bridged to the 48 kHz pipeline by [`Resampler`].
fn device_f32_config(
    device: &cpal::Device,
    input: bool,
) -> Result<(cpal::StreamConfig, u32), String> {
    let name = device
        .name()
        .unwrap_or_else(|_| "unknown device".to_string());
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
    select_f32_config(&name, &ranges, input)
}

fn select_f32_config(
    name: &str,
    ranges: &[cpal::SupportedStreamConfigRange],
    input: bool,
) -> Result<(cpal::StreamConfig, u32), String> {
    let best = ranges
        .iter()
        .filter(|range| range.sample_format() == cpal::SampleFormat::F32)
        .map(|range| {
            let rate = 48_000u32.clamp(range.min_sample_rate().0, range.max_sample_rate().0);
            (rate, range)
        })
        .min_by_key(|(rate, range)| (rate.abs_diff(48_000), range.channels()));
    if let Some((rate, range)) = best {
        let mut config = (*range).with_sample_rate(cpal::SampleRate(rate)).config();
        config.buffer_size = cpal::BufferSize::Default;
        return Ok((config, rate));
    }
    let side = if input {
        "microphone"
    } else {
        "audio output device"
    };
    if ranges.is_empty() {
        if input {
            // macOS hides input formats from processes that are denied (or
            // cannot be granted) microphone permission, so an empty format
            // list on a Mac with a working microphone points at permission,
            // not hardware.
            return Err(format!(
                "macOS reports no formats for the microphone \"{name}\". If this machine has \
                 a working microphone, Tyde likely lacks microphone permission (System \
                 Settings → Privacy & Security → Microphone)."
            ));
        }
        return Err(format!("The {side} \"{name}\" reports no audio formats"));
    }
    let formats = ranges
        .iter()
        .map(|range| {
            format!(
                "{:?} @ {}-{} Hz, {}ch",
                range.sample_format(),
                range.min_sample_rate().0,
                range.max_sample_rate().0,
                range.channels()
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    Err(format!(
        "The {side} \"{name}\" offers no f32 format (available: {formats})"
    ))
}

/// 150 ms at the 48 kHz pipeline rate. Enough headroom to absorb SSH-bridge
/// and webview-event bursts without adding conversational latency a person
/// would notice.
const JITTER_TARGET_SAMPLES: usize = 7_200;

/// Playback shock absorber. Voice audio reaches the shell over an SSH bridge
/// and the webview event path, both of which deliver in bursts under load;
/// playing the instant packets arrive turns every late burst into an audible
/// gap. After any underrun (and at session start) the gate holds silence
/// until the playback queue rebuilds `target_samples` of headroom.
struct JitterGate {
    buffering: bool,
    target_samples: usize,
}

impl JitterGate {
    fn new(target_samples: usize) -> Self {
        Self {
            buffering: true,
            target_samples,
        }
    }

    fn reset(&mut self) {
        self.buffering = true;
    }

    fn next(&mut self, playback: &mut VecDeque<f32>) -> f32 {
        if self.buffering {
            if playback.len() >= self.target_samples {
                self.buffering = false;
            } else {
                return 0.0;
            }
        }
        match playback.pop_front() {
            Some(sample) => sample,
            None => {
                self.buffering = true;
                0.0
            }
        }
    }
}

/// Streaming sample-rate converter bridging device streams to the fixed
/// 48 kHz AEC/Opus pipeline. Built for speech: linear interpolation, plus a
/// moving-average low-pass sized to the decimation ratio to bound aliasing
/// when converting downward.
struct Resampler {
    /// Source samples per output sample.
    step: f64,
    /// Source-stream position of the next output sample.
    pos: f64,
    /// Number of source samples pushed so far.
    src_pushed: u64,
    window: VecDeque<f32>,
    window_len: usize,
    window_sum: f64,
    prev_filtered: f32,
    curr_filtered: f32,
    passthrough: bool,
}

impl Resampler {
    fn new(src_rate: u32, dst_rate: u32) -> Self {
        let step = f64::from(src_rate) / f64::from(dst_rate);
        let window_len = (step.ceil() as usize).max(1);
        Self {
            step,
            pos: 0.0,
            src_pushed: 0,
            window: VecDeque::with_capacity(window_len),
            window_len,
            window_sum: 0.0,
            prev_filtered: 0.0,
            curr_filtered: 0.0,
            passthrough: src_rate == dst_rate,
        }
    }

    fn reset(&mut self) {
        self.pos = 0.0;
        self.src_pushed = 0;
        self.window.clear();
        self.window_sum = 0.0;
        self.prev_filtered = 0.0;
        self.curr_filtered = 0.0;
    }

    fn push(&mut self, sample: f32, out: &mut VecDeque<f32>) {
        if self.passthrough {
            out.push_back(sample);
            return;
        }
        self.window.push_back(sample);
        self.window_sum += f64::from(sample);
        if self.window.len() > self.window_len
            && let Some(oldest) = self.window.pop_front()
        {
            self.window_sum -= f64::from(oldest);
        }
        self.prev_filtered = self.curr_filtered;
        self.curr_filtered = (self.window_sum / self.window.len() as f64) as f32;
        let n = self.src_pushed as f64;
        self.src_pushed += 1;
        // Emit every output that falls in the source interval (n-1, n]; the
        // eager emission on each push keeps `pos` inside that interval, so
        // only the last two filtered values are ever needed.
        while self.pos <= n {
            let frac = (self.pos - (n - 1.0)).clamp(0.0, 1.0) as f32;
            out.push_back(self.prev_filtered + (self.curr_filtered - self.prev_filtered) * frac);
            self.pos += self.step;
        }
    }
}
