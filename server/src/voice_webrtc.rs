use std::collections::VecDeque;
use std::io::ErrorKind;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use opus::{Application, Channels, Decoder, Encoder};
use str0m::change::SdpOffer;
use str0m::format::Codec;
use str0m::media::{Frequency, MediaTime};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, RtcConfig};
use tokio::sync::{mpsc, oneshot};

use crate::voice::{
    VoiceMediaEvent, VoiceMediaFactory, VoiceMediaFuture, VoiceMediaSession, VoicePcmFrame,
    VoiceRuntimeError,
};
use protocol::VoiceIceCandidate;

const MEDIA_QUEUE_CAPACITY: usize = 64;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_BUFFERED_PCM_SAMPLES: usize = 48_000 * 10;
const OPUS_FRAME_SAMPLES: usize = 960;
const OPUS_FRAME_DURATION: Duration = Duration::from_millis(20);

pub(crate) struct Str0mMediaFactory;

impl VoiceMediaFactory for Str0mMediaFactory {
    fn open(&self) -> VoiceMediaFuture<'_, Box<dyn VoiceMediaSession>> {
        Box::pin(async {
            tokio::task::spawn_blocking(open_str0m)
                .await
                .map_err(|_| VoiceRuntimeError::Unavailable)?
        })
    }
}

fn open_str0m() -> Result<Box<dyn VoiceMediaSession>, VoiceRuntimeError> {
    install_crypto();
    let route = UdpSocket::bind("0.0.0.0:0").map_err(|_| VoiceRuntimeError::Unavailable)?;
    route
        .connect("192.0.2.1:9")
        .map_err(|_| VoiceRuntimeError::Unavailable)?;
    let local_ip = route
        .local_addr()
        .map_err(|_| VoiceRuntimeError::Unavailable)?
        .ip();
    drop(route);
    if !matches!(local_ip, IpAddr::V4(_) | IpAddr::V6(_)) || local_ip.is_loopback() {
        return Err(VoiceRuntimeError::Unavailable);
    }
    let socket = UdpSocket::bind(SocketAddr::new(local_ip, 0))
        .map_err(|_| VoiceRuntimeError::Unavailable)?;
    socket
        .set_nonblocking(true)
        .map_err(|_| VoiceRuntimeError::Unavailable)?;
    let local_addr = socket
        .local_addr()
        .map_err(|_| VoiceRuntimeError::Unavailable)?;
    let (command_tx, command_rx) = mpsc::channel(MEDIA_QUEUE_CAPACITY);
    let (event_tx, event_rx) = mpsc::channel(MEDIA_QUEUE_CAPACITY);
    let (audio_tx, audio_rx) = mpsc::channel(MEDIA_QUEUE_CAPACITY);
    let closed = Arc::new(AtomicBool::new(false));
    let thread_closed = Arc::clone(&closed);
    std::thread::Builder::new()
        .name("tyde-voice-webrtc".to_owned())
        .spawn(move || {
            run_webrtc(
                socket,
                local_addr,
                command_rx,
                event_tx,
                audio_tx,
                thread_closed,
            )
        })
        .map_err(|_| VoiceRuntimeError::Unavailable)?;
    Ok(Box::new(Str0mMediaSession {
        command_tx,
        event_rx: Some(event_rx),
        audio_rx: Some(audio_rx),
        closed,
    }))
}

fn install_crypto() {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    INSTALLED.call_once(|| str0m::crypto::from_feature_flags().install_process_default());
}

enum MediaCommand {
    AcceptOffer(String, oneshot::Sender<Result<String, VoiceRuntimeError>>),
    AddCandidate(
        VoiceIceCandidate,
        oneshot::Sender<Result<(), VoiceRuntimeError>>,
    ),
    EndCandidates(oneshot::Sender<Result<(), VoiceRuntimeError>>),
    Play(VoicePcmFrame),
    Close,
}

struct Str0mMediaSession {
    command_tx: mpsc::Sender<MediaCommand>,
    event_rx: Option<mpsc::Receiver<VoiceMediaEvent>>,
    audio_rx: Option<mpsc::Receiver<VoicePcmFrame>>,
    closed: Arc<AtomicBool>,
}

impl VoiceMediaSession for Str0mMediaSession {
    fn accept_offer<'a>(&'a mut self, offer: &'a str) -> VoiceMediaFuture<'a, String> {
        Box::pin(async move {
            let (reply_tx, reply_rx) = oneshot::channel();
            self.command_tx
                .try_send(MediaCommand::AcceptOffer(offer.to_owned(), reply_tx))
                .map_err(|_| VoiceRuntimeError::Closed)?;
            tokio::time::timeout(COMMAND_TIMEOUT, reply_rx)
                .await
                .map_err(|_| VoiceRuntimeError::Closed)?
                .map_err(|_| VoiceRuntimeError::Closed)?
        })
    }

    fn add_ice_candidate<'a>(
        &'a mut self,
        candidate: &'a VoiceIceCandidate,
    ) -> VoiceMediaFuture<'a, ()> {
        Box::pin(async move {
            let (reply_tx, reply_rx) = oneshot::channel();
            self.command_tx
                .try_send(MediaCommand::AddCandidate(candidate.clone(), reply_tx))
                .map_err(|_| VoiceRuntimeError::Closed)?;
            tokio::time::timeout(COMMAND_TIMEOUT, reply_rx)
                .await
                .map_err(|_| VoiceRuntimeError::Closed)?
                .map_err(|_| VoiceRuntimeError::Closed)?
        })
    }

    fn end_ice_candidates(&mut self) -> VoiceMediaFuture<'_, ()> {
        Box::pin(async move {
            let (reply_tx, reply_rx) = oneshot::channel();
            self.command_tx
                .try_send(MediaCommand::EndCandidates(reply_tx))
                .map_err(|_| VoiceRuntimeError::Closed)?;
            tokio::time::timeout(COMMAND_TIMEOUT, reply_rx)
                .await
                .map_err(|_| VoiceRuntimeError::Closed)?
                .map_err(|_| VoiceRuntimeError::Closed)?
        })
    }

    fn take_input_audio(&mut self) -> Option<mpsc::Receiver<VoicePcmFrame>> {
        self.audio_rx.take()
    }

    fn take_events(&mut self) -> Option<mpsc::Receiver<VoiceMediaEvent>> {
        self.event_rx.take()
    }

    fn play_output_audio(&mut self, frame: VoicePcmFrame) -> Result<(), VoiceRuntimeError> {
        match self.command_tx.try_send(MediaCommand::Play(frame)) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => {
                tracing::warn!(
                    "dropping Nova audio frame because the WebRTC bridge is backpressured"
                );
                Ok(())
            }
            Err(mpsc::error::TrySendError::Closed(_)) => Err(VoiceRuntimeError::Closed),
        }
    }

    fn close(&mut self) {
        self.closed.store(true, Ordering::Release);
        let _ = self.command_tx.try_send(MediaCommand::Close);
    }
}

impl Drop for Str0mMediaSession {
    fn drop(&mut self) {
        self.close();
    }
}

fn run_webrtc(
    socket: UdpSocket,
    local_addr: SocketAddr,
    mut command_rx: mpsc::Receiver<MediaCommand>,
    event_tx: mpsc::Sender<VoiceMediaEvent>,
    audio_tx: mpsc::Sender<VoicePcmFrame>,
    closed: Arc<AtomicBool>,
) {
    let mut rtc = RtcConfig::new().build(Instant::now());
    let Ok(candidate) = Candidate::host(local_addr, "udp") else {
        return;
    };
    if rtc.add_local_candidate(candidate).is_none() {
        return;
    }
    let Ok(mut encoder) = Encoder::new(48_000, Channels::Mono, Application::Voip) else {
        return;
    };
    let Ok(mut decoder) = Decoder::new(48_000, Channels::Mono) else {
        return;
    };
    let mut media_mid = None;
    let mut outgoing_pcm = VecDeque::new();
    let mut media_time = 0_u64;
    let mut next_send_at = None;
    let mut receive_buffer = vec![0_u8; 2_000];
    let mut drain_before_mutation = false;
    let mut remote_ice = RemoteIceState::Open;

    loop {
        if closed.load(Ordering::Acquire) {
            return;
        }
        let permit_mutation = !drain_before_mutation;
        drain_before_mutation = false;
        if permit_mutation && let Ok(command) = command_rx.try_recv() {
            match command {
                MediaCommand::AcceptOffer(sdp, reply) => {
                    let result = SdpOffer::from_sdp_string(&sdp)
                        .map_err(|_| VoiceRuntimeError::InvalidSignal)
                        .and_then(|offer| {
                            rtc.sdp_api()
                                .accept_offer(offer)
                                .map(|answer| mark_local_ice_complete(answer.to_sdp_string()))
                                .map_err(|_| VoiceRuntimeError::InvalidSignal)
                        });
                    let _ = reply.send(result);
                }
                MediaCommand::AddCandidate(candidate, reply) => {
                    if remote_ice.accept_candidate() {
                        tracing::debug!("accepting a remote ICE candidate that raced completion");
                    }
                    let result = Candidate::from_sdp_string(&candidate.candidate)
                        .map_err(|_| VoiceRuntimeError::InvalidSignal)
                        .map(|candidate| {
                            rtc.add_remote_candidate(candidate);
                        });
                    let _ = reply.send(result);
                }
                MediaCommand::EndCandidates(reply) => {
                    remote_ice.complete();
                    let _ = reply.send(Ok(()));
                }
                MediaCommand::Play(frame) => {
                    append_48khz(&mut outgoing_pcm, &frame);
                    if outgoing_pcm.len() > MAX_BUFFERED_PCM_SAMPLES {
                        let overflow = outgoing_pcm.len() - MAX_BUFFERED_PCM_SAMPLES;
                        let drop_samples =
                            overflow.div_ceil(OPUS_FRAME_SAMPLES) * OPUS_FRAME_SAMPLES;
                        outgoing_pcm.drain(..drop_samples.min(outgoing_pcm.len()));
                        tracing::warn!(
                            drop_samples,
                            "dropping oldest buffered Nova audio under WebRTC backpressure"
                        );
                    }
                }
                MediaCommand::Close => return,
            }
        }

        let now = Instant::now();
        if permit_mutation
            && outgoing_pcm.len() >= OPUS_FRAME_SAMPLES
            && let Some(mid) = media_mid
            && next_send_at.is_none_or(|deadline| deadline <= now)
        {
            let pcm: Vec<_> = outgoing_pcm.drain(..OPUS_FRAME_SAMPLES).collect();
            let mut encoded = [0_u8; 1_500];
            let Ok(length) = encoder.encode(&pcm, &mut encoded) else {
                return;
            };
            let Some(writer) = rtc.writer(mid) else {
                return;
            };
            let Some(pt) = writer
                .payload_params()
                .find(|params| params.spec().codec == Codec::Opus)
                .map(|params| params.pt())
            else {
                return;
            };
            if writer
                .write(
                    pt,
                    now,
                    MediaTime::new(media_time, Frequency::FORTY_EIGHT_KHZ),
                    &encoded[..length],
                )
                .is_err()
            {
                return;
            }
            media_time = media_time.saturating_add(OPUS_FRAME_SAMPLES as u64);
            next_send_at = Some(next_rtp_deadline(now));
        }

        loop {
            match rtc.poll_output() {
                Ok(Output::Transmit(transmit)) => {
                    let _ = socket.send_to(&transmit.contents, transmit.destination);
                }
                Ok(Output::Event(Event::Connected)) => {
                    if event_tx.try_send(VoiceMediaEvent::Connected).is_err() {
                        return;
                    }
                }
                Ok(Output::Event(Event::IceConnectionStateChange(
                    IceConnectionState::Disconnected,
                ))) => {
                    let _ = event_tx.try_send(VoiceMediaEvent::Failed);
                    return;
                }
                Ok(Output::Event(Event::MediaAdded(media))) => {
                    if media.kind.is_audio() {
                        media_mid = Some(media.mid);
                    }
                }
                Ok(Output::Event(Event::MediaData(data)))
                    if data.params.spec().codec == Codec::Opus =>
                {
                    let mut decoded = [0_i16; 5_760];
                    let Ok(length) = decoder.decode(&data.data, &mut decoded, false) else {
                        continue;
                    };
                    let samples = resample_bandlimited(&decoded[..length], 48_000, 16_000);
                    if audio_tx
                        .try_send(VoicePcmFrame {
                            sample_rate_hertz: 16_000,
                            samples,
                        })
                        .is_err()
                    {
                        return;
                    }
                }
                Ok(Output::Event(_)) => {}
                Ok(Output::Timeout(deadline)) => {
                    if deadline <= Instant::now() {
                        if rtc.handle_input(Input::Timeout(Instant::now())).is_err() {
                            return;
                        }
                        drain_before_mutation = true;
                    }
                    break;
                }
                Err(_) => return,
            }
        }

        if !permit_mutation || drain_before_mutation {
            continue;
        }
        match socket.recv_from(&mut receive_buffer) {
            Ok((length, source)) => {
                let receive = Receive {
                    proto: Protocol::Udp,
                    source,
                    destination: local_addr,
                    contents: match receive_buffer[..length].try_into() {
                        Ok(contents) => contents,
                        Err(_) => continue,
                    },
                };
                if rtc
                    .handle_input(Input::Receive(Instant::now(), receive))
                    .is_err()
                {
                    return;
                }
                drain_before_mutation = true;
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(_) => return,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoteIceState {
    Open,
    Complete,
}

impl RemoteIceState {
    fn accept_candidate(&mut self) -> bool {
        let raced_completion = *self == Self::Complete;
        *self = Self::Open;
        raced_completion
    }

    fn complete(&mut self) {
        *self = Self::Complete;
    }
}

fn next_rtp_deadline(sent_at: Instant) -> Instant {
    sent_at + OPUS_FRAME_DURATION
}

fn mark_local_ice_complete(mut answer: String) -> String {
    if !answer.contains("a=end-of-candidates") {
        if !answer.ends_with("\r\n") {
            answer.push_str("\r\n");
        }
        answer.push_str("a=end-of-candidates\r\n");
    }
    answer
}

fn append_48khz(output: &mut VecDeque<i16>, frame: &VoicePcmFrame) {
    output.extend(resample_bandlimited(
        &frame.samples,
        frame.sample_rate_hertz,
        48_000,
    ));
}

fn resample_bandlimited(input: &[i16], input_rate: u32, output_rate: u32) -> Vec<i16> {
    if input.is_empty() || input_rate == 0 || output_rate == 0 {
        return Vec::new();
    }
    if input_rate == output_rate {
        return input.to_vec();
    }
    const HALF_TAPS: isize = 24;
    let output_len = input
        .len()
        .saturating_mul(output_rate as usize)
        .div_ceil(input_rate as usize);
    let cutoff = (output_rate as f64 / input_rate as f64).min(1.0) * 0.94;
    let ratio = input_rate as f64 / output_rate as f64;
    let mut output = Vec::with_capacity(output_len);
    for output_index in 0..output_len {
        let source = output_index as f64 * ratio;
        let center = source.floor() as isize;
        let mut weighted = 0.0;
        let mut weight_sum = 0.0;
        for tap in (center - HALF_TAPS + 1)..=(center + HALF_TAPS) {
            if !(0..input.len() as isize).contains(&tap) {
                continue;
            }
            let distance = source - tap as f64;
            let normalized = distance / HALF_TAPS as f64;
            if normalized.abs() >= 1.0 {
                continue;
            }
            let sinc_position = std::f64::consts::PI * cutoff * distance;
            let sinc = if sinc_position.abs() < f64::EPSILON {
                1.0
            } else {
                sinc_position.sin() / sinc_position
            };
            let window = 0.42
                + 0.5 * (std::f64::consts::PI * normalized).cos()
                + 0.08 * (2.0 * std::f64::consts::PI * normalized).cos();
            let weight = cutoff * sinc * window;
            weighted += input[tap as usize] as f64 * weight;
            weight_sum += weight;
        }
        let sample = if weight_sum.abs() < f64::EPSILON {
            0.0
        } else {
            weighted / weight_sum
        };
        output.push(sample.round().clamp(i16::MIN as f64, i16::MAX as f64) as i16);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hermetic_resampler_produces_opus_frame_rate() {
        let mut output = VecDeque::new();
        append_48khz(
            &mut output,
            &VoicePcmFrame {
                sample_rate_hertz: 24_000,
                samples: vec![1; 480],
            },
        );
        assert_eq!(output.len(), 960);
    }

    #[test]
    fn downsampler_attenuates_energy_above_the_destination_nyquist_limit() {
        fn tone(frequency: f64) -> Vec<i16> {
            (0..4_800)
                .map(|sample| {
                    ((2.0 * std::f64::consts::PI * frequency * sample as f64 / 48_000.0).sin()
                        * 12_000.0) as i16
                })
                .collect()
        }
        fn rms(samples: &[i16]) -> f64 {
            let interior = &samples[100..samples.len() - 100];
            (interior
                .iter()
                .map(|sample| (*sample as f64).powi(2))
                .sum::<f64>()
                / interior.len() as f64)
                .sqrt()
        }
        let passband = resample_bandlimited(&tone(1_000.0), 48_000, 16_000);
        let rejected = resample_bandlimited(&tone(12_000.0), 48_000, 16_000);
        assert!(rms(&rejected) < rms(&passband) * 0.1);
    }

    #[test]
    fn upsampler_suppresses_spectral_images() {
        fn amplitude(samples: &[i16], sample_rate: f64, frequency: f64) -> f64 {
            let interior = &samples[200..samples.len() - 200];
            let (sin, cos) =
                interior
                    .iter()
                    .enumerate()
                    .fold((0.0, 0.0), |(sin, cos), (index, sample)| {
                        let phase =
                            2.0 * std::f64::consts::PI * frequency * index as f64 / sample_rate;
                        (
                            sin + *sample as f64 * phase.sin(),
                            cos + *sample as f64 * phase.cos(),
                        )
                    });
            sin.hypot(cos) / interior.len() as f64
        }
        let input: Vec<i16> = (0..1_600)
            .map(|sample| {
                ((2.0 * std::f64::consts::PI * 1_000.0 * sample as f64 / 16_000.0).sin() * 12_000.0)
                    as i16
            })
            .collect();
        let output = resample_bandlimited(&input, 16_000, 48_000);
        let fundamental = amplitude(&output, 48_000.0, 1_000.0);
        let image = amplitude(&output, 48_000.0, 15_000.0);
        assert!(image < fundamental * 0.1);
    }

    #[test]
    fn rtp_deadlines_never_schedule_catch_up_bursts() {
        let sent_at = Instant::now();
        assert_eq!(
            next_rtp_deadline(sent_at).duration_since(sent_at),
            OPUS_FRAME_DURATION
        );
    }

    #[test]
    fn ice_completion_is_idempotent_and_accepts_a_racing_candidate() {
        let mut state = RemoteIceState::Open;
        state.complete();
        state.complete();
        assert!(state.accept_candidate());
        assert_eq!(state, RemoteIceState::Open);
        assert!(!state.accept_candidate());
    }

    #[test]
    fn answer_marks_the_pregathered_host_candidate_complete_once() {
        let answer =
            mark_local_ice_complete("v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\n".to_owned());
        assert_eq!(answer.matches("a=end-of-candidates").count(), 1);
        assert_eq!(mark_local_ice_complete(answer.clone()), answer);
    }

    #[test]
    fn hermetic_opus_path_encodes_and_decodes_one_audio_track_frame() {
        let mut encoder =
            Encoder::new(48_000, Channels::Mono, Application::Voip).expect("create Opus encoder");
        let mut decoder = Decoder::new(48_000, Channels::Mono).expect("create Opus decoder");
        let pcm: Vec<i16> = (0..960)
            .map(|sample| ((sample as f32 * 0.08).sin() * 8_000.0) as i16)
            .collect();
        let mut encoded = [0_u8; 1_500];
        let encoded_len = encoder.encode(&pcm, &mut encoded).expect("encode Opus");
        assert!(encoded_len > 0);
        let mut decoded = [0_i16; 960];
        let decoded_len = decoder
            .decode(&encoded[..encoded_len], &mut decoded, false)
            .expect("decode Opus");
        assert_eq!(decoded_len, 960);
        assert!(decoded.iter().any(|sample| *sample != 0));
    }
}
