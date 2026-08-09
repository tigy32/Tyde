# Native Voice Protocol

Native voice is an ordered protocol feature over the normal Tyde byte stream.
It does not introduce WebRTC networking, UDP, ICE, STUN, TURN, or mDNS.

## Capability and ownership

The server emits `voice_capabilities` after connection setup. The UI shows the
voice affordance only when the host setting is enabled, Nova is available, and
the selected agent has a live `instance_stream`. A session owns the exact
`VoiceTarget { agent_id, instance_stream }`; changing agents ends it and never
auto-starts another session.

`voice_start` proposes Opus formats and a strictly increasing generation.
`voice_accepted` is sent only after the provider stream is live and fixes the
session id, generation, target, and formats. Desktop and mobile must not open a
microphone before this acceptance. All later frames use
`/voice/<session-id>` and repeat session id and generation.

States are `starting`, `listening`, `agent_working`, `speaking`,
`interrupting`, `ending`, and `ended`. Controls are `voice_input_end`,
`voice_interrupt`, and `voice_stop`. Server output includes typed transcripts,
state/progress, output-start, audio, stop statistics, and errors. Any foreign
target, future/unknown generation or session, wrong direction, invalid format,
or malformed packet table is rejected. Frames from a completed older
generation are discarded without affecting the current generation; duplicate
media is counted as dropped.

## Media

`voice_audio` uses a binary record body and a JSON `VoiceAudioPayload` header:

- direction (`input` or `output`)
- first media sequence and 48 kHz sample-clock timestamp
- one to three `u16` Opus packet lengths whose exact sum is the body length
- Opus, mono, 20 ms packets; 48 kHz client capture and 24 kHz provider output

The server decodes capture directly to 16 kHz with libopus at the Nova
boundary and encodes Nova's 24 kHz PCM directly with libopus. There is no
standalone resampler. Provider PCM exists only inside the server adapter.

The connection writer has separate bounded control (64), chat (256), bulk
(256), and audio lanes. Control is highest priority; after eight consecutive
audio records the scheduler forces chat/bulk progress and alternates those
lower lanes. Audio drops oldest packets
to remain at eight packets (about 160 ms). A stop or interrupt purges queued
audio for that generation before write. Sequence numbers are assigned only
after scheduler selection, so drops cannot create holes. Large bulk JSON is
fragmented and urgent lanes may interleave between fragment records.

Audio logs are sampled once per 256 frames and audio never enters the protocol
diagnostic history ring. Stop payloads carry admitted/dropped packets, bytes,
and queue high-water information.

## Clients

The Tauri shell starts a dedicated native-audio control thread. That thread,
not Tauri managed state, owns every CPAL device/stream, AEC processor, Opus
encoder/decoder, and active-session identity. Tauri retains only bounded
`Send + Sync` command/event handles. The thread creates a fresh capture/output
pair only after it acknowledges the exact server acceptance authorization.
The WebView sees small Opus packets and controls only, never raw PCM. Stop,
target switch, transport loss, and shell exit wait for an explicit
acknowledgement sent only after both streams have been dropped. Every wait is
bounded; timeout marks the control handle fail-closed rather than blocking the
UI indefinitely.

The bundled `webrtc-audio-processing-sys` build requires Meson and Ninja. The
workspace patches that crate narrowly so its build script invokes
`tools/native-build-tool.py` instead of searching the ambient `PATH`. The
upstream build script hard-codes `Command::new("meson")` and
`Command::new("ninja")` and exposes no Meson/Ninja executable override, which
is why the owned patch is required. The wrapper acquires a cross-process
repository lock, verifies or lazily provisions
repository-local Meson 1.11.2 and Ninja 1.11.1.4, then executes the pinned tool
by absolute path. A verified cache performs no package-manager or network work,
so subsequent builds work offline. Paths are passed as argument arrays and may
contain spaces or shell metacharacters. This applies equally to ordinary Cargo
builds, `cargo tauri dev`, debug instances, checks, CI, and release builds; none
depends on a global install or manual `PATH` changes. Meson 1.7 generated the
removed `_LIBCPP_ENABLE_ASSERTIONS` define with current Apple Clang. The
upstream Meson Apple-Clang fix uses the supported libc++ hardening mode instead
([Meson #14548](https://github.com/mesonbuild/meson/pull/14548),
[libc++ 20 notes](https://libcxx.llvm.org/ReleaseNotes/20.html)). Pinning a
Meson release containing that fix preserves the maintained, real WebRTC AEC
implementation instead of weakening DSP or requiring an older Xcode.
Check CI and release jobs eagerly invoke the same provisioner for clearer stage
diagnostics, while the dependency wrapper remains the authoritative lazy guard
for every entry point. Cache explanation mode remains read-only and does not
provision these tools.

The AEC Meson fallback is also self-contained. The exact Abseil 20240722.0
source archive and WrapDB `20240722.0-3` patch archive live in the vendored
subproject package cache with their upstream SHA-256 values, sizes,
Apache-2.0/MIT licenses, and provenance. Meson uses
`--wrap-mode=nodownload` and forces all six requested Abseil dependencies
(`absl_base`, `absl_flags`, `absl_strings`, `absl_numeric`,
`absl_synchronization`, and `absl_bad_optional_access`) to the fallback. A
missing or modified cache therefore fails without a network attempt, and no
dependency can be mixed with an ambient system Abseil. A partial target-local
AEC configure is removed, or retried once from a clean AEC build directory
after reconfigure failure; recovery is bounded to that crate's `OUT_DIR`.

## Nova and tools

Nova uses the server's configured AWS profile, region, exact model
(`amazon.nova-2-sonic-v1:0` by default), and turn-ending sensitivity. Turn
ending defaults to low sensitivity (the patient, roughly two-second pause) and
can be changed to medium or high in Voice settings. Missing or expired
credentials and model/region failures are typed and visible; there is no model
or provider downgrade. AWS event-stream base64 occurs only at that provider
boundary.

Nova's single tool sends substantive work through the existing agent handle.
The voice layer validates the target instance, attaches the normal agent event
stream before delivery, correlates `StreamStart`/`StreamEnd` by `message_id`,
resets a 90-second inactivity deadline on each event, enforces a 450-second
total bound, and returns the completed assistant message to Nova as the tool
result. Periodic working updates are UI-only because Nova accepts one system
content block per prompt and the tool protocol has no partial-result event.
The backend agent remains voice-unaware and is not cancelled by a voice
interruption.

## Validation

The synthetic suite uses the real `TYD2` reader/writer, production scheduler,
real rumqttd encrypted byte stream, mock Nova/provider events, normal agent
tool bridge, and desktop/mobile lifecycle adapters. The M7 case moves a real
4 MiB bulk envelope while admitting audio and control and requires both to
arrive before complete bulk reassembly with local latency at or below 100 ms.
Real AWS coverage remains ignored and is
enabled only by the repository's explicit paid-live-test gate.
